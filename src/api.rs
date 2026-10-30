use crate::{
    config::Config,
    core::{self, Action},
    error::Error,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;
async fn deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match tokio::time::timeout(std::time::Duration::from_secs(120),next.run(request)).await{
        Ok(response)=>response,
        Err(_)=>(StatusCode::REQUEST_TIMEOUT,Json(json!({"ok":false,"error":Error::new("request_timeout","Ebook request exceeded its deadline; retry with the same idempotency key",4)}))).into_response(),
    }
}
#[derive(Clone)]
struct Api {
    config: Arc<Config>,
    slots: Arc<tokio::sync::Semaphore>,
}
fn authorized(headers: &HeaderMap, c: &Config) -> bool {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    bool::from(Sha256::digest(provided.as_bytes()).ct_eq(&Sha256::digest(c.api_token.as_bytes())))
}
async fn operation(
    State(api): State<Api>,
    headers: HeaderMap,
    payload: std::result::Result<Json<Action>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<Value>) {
    if !authorized(&headers, &api.config) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":Error::new("unauthorized","Bearer token required",5)})),
        );
    }
    let action = match payload {
        Ok(Json(v)) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"ok":false,"error":Error::invalid("Invalid operation JSON or request too large")}),
                ),
            );
        }
    };
    let permit = match api.slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(
                    json!({"ok":false,"error":Error::new("busy","Too many concurrent ebook operations",4)}),
                ),
            );
        }
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        core::execute(&api.config, action)
    })
    .await
    {
        Ok(Ok(v)) => (StatusCode::OK, Json(json!({"ok":true,"data":v}))),
        Ok(Err(e)) => {
            let status = match e.exit {
                2 => StatusCode::BAD_REQUEST,
                3 => StatusCode::NOT_FOUND,
                4 => StatusCode::BAD_GATEWAY,
                5 => StatusCode::CONFLICT,
                6 => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({"ok":false,"error":e})))
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"ok":false,"error":Error::new("internal_error","Ebook operation terminated unexpectedly",7)}),
            ),
        ),
    }
}
pub fn serve(c: Config, listen: std::net::SocketAddr) -> crate::error::Result<()> {
    if !listen.ip().is_loopback() {
        return Err(Error::invalid(
            "API must bind to loopback; use an authenticated SSH tunnel for private remote access",
        ));
    }
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let app = Router::new()
            .route("/health/live", get(|| async { Json(json!({"live":true})) }))
            .route("/v1/operations", post(operation))
            .layer(DefaultBodyLimit::max(8192))
            .layer(axum::middleware::from_fn(deadline))
            .with_state(Api {
                config: Arc::new(c),
                slots: Arc::new(tokio::sync::Semaphore::new(2)),
            });
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
