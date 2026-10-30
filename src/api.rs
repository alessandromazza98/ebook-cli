use crate::{
    config::Config,
    core::{self, Action},
    error::Error,
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Owner,
    Client,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Client => "client",
        }
    }
}
#[derive(Default)]
struct RateLimit {
    requests: VecDeque<Instant>,
}
impl RateLimit {
    fn check(&mut self, now: Instant, limit: u32) -> Option<u64> {
        while self
            .requests
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            self.requests.pop_front();
        }
        if self.requests.len() >= limit as usize {
            return Some(
                60 - now
                    .duration_since(*self.requests.front().unwrap())
                    .as_secs(),
            );
        }
        self.requests.push_back(now);
        None
    }
}
#[derive(Clone)]
struct Api {
    config: Arc<Config>,
    slots: Arc<tokio::sync::Semaphore>,
    limits: Arc<Mutex<[RateLimit; 2]>>,
}
fn failure(status: StatusCode, error: Error) -> Response {
    (status, Json(json!({"ok":false,"error":error}))).into_response()
}
fn authorized(headers: &HeaderMap, c: &Config) -> Option<Role> {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))?;
    let digest = Sha256::digest(provided.as_bytes());
    if bool::from(digest.ct_eq(&Sha256::digest(c.api_token.as_bytes()))) {
        return Some(Role::Owner);
    }
    c.api
        .client_token
        .as_ref()
        .filter(|token| bool::from(digest.ct_eq(&Sha256::digest(token.as_bytes()))))
        .map(|_| Role::Client)
}
// Authentication and admission precede JSON extraction or any upstream work.
async fn authenticate(State(api): State<Api>, mut request: Request, next: Next) -> Response {
    let Some(role) = authorized(request.headers(), &api.config) else {
        let mut response = failure(
            StatusCode::UNAUTHORIZED,
            Error::new("unauthorized", "Bearer token required", 5),
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        return response;
    };
    let wait = match api.limits.lock() {
        Ok(mut limits) => limits[if role == Role::Owner { 0 } else { 1 }]
            .check(Instant::now(), api.config.api.requests_per_minute),
        Err(_) => {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                Error::new("internal_error", "API rate limiter unavailable", 7),
            );
        }
    };
    if let Some(wait) = wait {
        let mut error = Error::new(
            "rate_limited",
            "API request limit reached; wait for Retry-After before retrying",
            4,
        );
        error.retryable = true;
        let mut response = failure(StatusCode::TOO_MANY_REQUESTS, error);
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, wait.to_string().parse().unwrap());
        return response;
    }
    request.extensions_mut().insert(role);
    next.run(request).await
}
async fn deadline(request: Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(120), next.run(request)).await {
        Ok(response) => response,
        Err(_) => failure(
            StatusCode::REQUEST_TIMEOUT,
            Error::new(
                "request_timeout",
                "Ebook request exceeded its deadline; retry with the same idempotency key",
                4,
            ),
        ),
    }
}
fn help_document(c: &Config, role: Role) -> Value {
    let mut actions = vec!["search", "request", "select", "jobs", "status"];
    if role == Role::Owner {
        actions.extend(["retry", "confirm_delivery"]);
    }
    json!({
        "api_version": env!("CARGO_PKG_VERSION"),
        "purpose": "Find EPUB books, download through qBittorrent, prepare them and email them to the server-configured Kindle.",
        "delivery": {
            "configured": !c.smtp.host.is_empty() && !c.smtp.from.is_empty() && !c.smtp.kindle.is_empty(),
            "destination": "server_configured_kindle",
            "destination_address_required_from_caller": false,
            "instruction": "The server owns the recipient and SMTP configuration. Do not ask for a Kindle address or pass one in a request. Configured does not mean a live SMTP or Kindle-delivery check has passed."
        },
        "authentication": {"scheme":"Bearer", "credential_role":role.name(), "allowed_actions":actions},
        "operations": {
            "search": "Read-only lookup. Returns releases and caches their IDs. It NEVER creates a job, downloads or sends a book.",
            "request": "Submit a title with optional author/language and an idempotency_key. A clear match creates or reuses a job; ambiguity returns clarification_required without creating a job.",
            "select": "Submit an eligible release_id from search or clarification candidates, language if needed, and an idempotency_key. Creates or reuses a job. The worker automatically downloads, prepares and sends it; no separate download/send call is needed.",
            "jobs": "List existing jobs. Does not create or start anything.",
            "status": "Read an existing job using its job_id. Does not create or start anything."
        },
        "workflow": [
            "For a user's request to send a book, call request, or search followed by select for the chosen eligible release.",
            "After clarification_required, ask only about the ambiguity described in reason/candidates, then select the user's choice.",
            "Only report that a job exists after receiving ok=true and data.job.id. Inspect data.job.state: a replay can return a completed job without sending again.",
            "Poll status for the returned job_id every 15–30 seconds until a terminal state. Downloads and SMTP run in the background.",
            "Report SMTP acceptance accurately and ask the user to confirm actual Kindle arrival. The restricted client cannot force retries or mark delivery confirmed."
        ],
        "http": {"method":"POST", "path":"/v1/operations", "content_type":"application/json", "client_timeout_seconds":130},
        "examples": {
            "request": {"action":"request", "title":"Alice's Adventures in Wonderland", "author":"Lewis Carroll", "language":"en", "idempotency_key":"REPLACE_WITH_A_UNIQUE_KEY_FOR_THIS_SUBMISSION"},
            "search": {"action":"search", "query":"Alice's Adventures in Wonderland"},
            "select": {"action":"select", "release_id":"REPLACE_WITH_RELEASE_ID_FROM_SEARCH", "language":"en", "idempotency_key":"REPLACE_WITH_A_UNIQUE_KEY_FOR_THIS_SUBMISSION"},
            "status": {"action":"status", "job_id":"REPLACE_WITH_RETURNED_JOB_ID"},
            "jobs": {"action":"jobs"}
        },
        "idempotency": "Use a unique key per intended request or selection. Reuse that key and body after a timeout or lost response. Existing release jobs are reused, including completed jobs; they are not sent again.",
        "states": {
            "in_progress":["queued", "downloading", "preparing", "ready", "sending"],
            "smtp_accepted":"SMTP accepted the email. This is not proof the book arrived on the Kindle. Stop polling and ask the user to verify arrival.",
            "delivered":"Delivery was confirmed by the owner.",
            "send_uncertain":"A send may have succeeded. Stop and ask the owner to check Kindle/email; do not resubmit with a new key or retry automatically.",
            "failed":"Report job.error.code and job.error.message. Owner intervention is required for retry.",
            "existing_workflow":"The torrent belongs to the existing workflow. Report this; do not change its category."
        },
        "limits": {"requests_per_minute":c.api.requests_per_minute, "new_jobs_per_rolling_24_hours":c.api.jobs_per_day, "max_torrent_bytes":c.max_torrent_bytes, "max_request_bytes":8192},
        "errors": {
            "reporting":"Report the exact HTTP status and error.code/message without credentials. If the client tool prevents a call, report that tool error; do not claim the server rejected a call it never received.",
            "rate_limited":"Wait for Retry-After seconds before retrying; preserve the submission key and body.",
            "busy":"Wait at least 15 seconds before retrying with the same key and body.",
            "daily_quota_exceeded":"Wait for earlier new jobs to leave the rolling 24-hour quota; status checks remain available.",
            "forbidden":"The credential does not allow this action; ask the owner for the appropriate local CLI operation.",
            "not_found":"For select, search first and use the returned release_id. For status, use the returned job ID."
        }
    })
}
async fn help(State(api): State<Api>, Extension(role): Extension<Role>) -> Json<Value> {
    Json(json!({"ok":true,"data":help_document(&api.config, role)}))
}
async fn operation(
    State(api): State<Api>,
    Extension(role): Extension<Role>,
    payload: std::result::Result<Json<Action>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let action = match payload {
        Ok(Json(v)) => v,
        Err(_) => {
            return failure(
                StatusCode::BAD_REQUEST,
                Error::invalid("Invalid operation JSON or request too large"),
            );
        }
    };
    let name = match &action {
        Action::Search { .. } => "search",
        Action::Select { .. } => "select",
        Action::Request { .. } => "request",
        Action::Jobs { .. } => "jobs",
        Action::Status { .. } => "status",
        Action::Retry { .. } => "retry",
        Action::ConfirmDelivery { .. } => "confirm_delivery",
    };
    if role == Role::Client
        && matches!(
            action,
            Action::Retry { .. } | Action::ConfirmDelivery { .. }
        )
    {
        eprintln!(
            "{}",
            json!({"event":"api_forbidden","client":role.name(),"action":name})
        );
        return failure(
            StatusCode::FORBIDDEN,
            Error::new(
                "forbidden",
                "This operation requires the owner credential or local CLI",
                5,
            ),
        );
    }
    let permit = match api.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return failure(
                StatusCode::TOO_MANY_REQUESTS,
                Error::new("busy", "Too many concurrent ebook operations", 4),
            );
        }
    };
    match tokio::task::spawn_blocking(move || {
        // Keep admission occupied even if the caller disconnects or times out.
        let _permit = permit;
        let result = core::execute_limited(&api.config, action, Some(api.config.api.jobs_per_day));
        match &result {
            Ok(v) if matches!(name, "select" | "request" | "retry" | "confirm_delivery") => {
                eprintln!("{}", json!({"event":"api_operation","client":role.name(),"action":name,"job_id":v["job"]["id"],"ok":true}));
            }
            Err(e) => eprintln!("{}", json!({"event":"api_operation","client":role.name(),"action":name,"ok":false,"error_code":e.code})),
            _ => {} // Successful polling/search is available in the proxy access log.
        }
        result
    }).await {
        Ok(Ok(v)) => Json(json!({"ok":true,"data":v})).into_response(),
        Ok(Err(e)) => {
            let status = if e.code == "daily_quota_exceeded" { StatusCode::TOO_MANY_REQUESTS } else {
                match e.exit {
                    2 => StatusCode::BAD_REQUEST, 3 => StatusCode::NOT_FOUND,
                    4 => StatusCode::BAD_GATEWAY, 5 => StatusCode::CONFLICT,
                    6 => StatusCode::UNPROCESSABLE_ENTITY, _ => StatusCode::INTERNAL_SERVER_ERROR,
                }
            };
            failure(status, e)
        }
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, Error::new("internal_error", "Ebook operation terminated unexpectedly", 7)),
    }
}
pub fn serve(c: Config, listen: std::net::SocketAddr) -> crate::error::Result<()> {
    if !listen.ip().is_loopback() {
        return Err(Error::invalid(
            "API must bind to loopback; use an HTTPS reverse proxy or authenticated SSH tunnel",
        ));
    }
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let api = Api {
            config: Arc::new(c),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            limits: Arc::new(Mutex::new([RateLimit::default(), RateLimit::default()])),
        };
        let app = Router::new()
            .route("/v1/operations", post(operation))
            .route("/v1/help", get(help))
            .route_layer(axum::middleware::from_fn_with_state(
                api.clone(),
                authenticate,
            ))
            .route("/health/live", get(|| async { Json(json!({"live":true})) }))
            .layer(DefaultBodyLimit::max(8192))
            .layer(axum::middleware::from_fn(deadline))
            .with_state(api);
        let listener = tokio::net::TcpListener::bind(listen).await?;
        eprintln!("{}", json!({"event":"api_started","listen":listen}));
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    eprintln!("Signal handler failed: {e}");
                }
            })
            .await
            .map_err(Into::into)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sliding_rate_limit_recovers_without_fixed_window_burst() {
        let mut limit = RateLimit::default();
        let now = Instant::now();
        assert_eq!(limit.check(now, 2), None);
        assert_eq!(limit.check(now + Duration::from_secs(59), 2), None);
        assert_eq!(limit.check(now + Duration::from_secs(59), 2), Some(1));
        assert_eq!(limit.check(now + Duration::from_secs(60), 2), None);
        assert_eq!(limit.check(now + Duration::from_secs(60), 2), Some(59));
    }
    #[test]
    fn credentials_are_distinct_and_client_can_be_revoked() {
        let mut c = Config::example();
        c.api.client_token = Some("client-secret-for-testing-only-123456".into());
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, &c).is_none());
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", c.api_token).parse().unwrap(),
        );
        assert!(authorized(&headers, &c) == Some(Role::Owner));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", c.api.client_token.as_ref().unwrap())
                .parse()
                .unwrap(),
        );
        assert!(authorized(&headers, &c) == Some(Role::Client));
        c.api.client_token = None;
        assert!(authorized(&headers, &c).is_none());
    }
}
