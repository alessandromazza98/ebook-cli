use crate::{
    config::{Config, language},
    db::{Db, Job},
    error::{Error, Result},
    integrations::{self, Release},
    selection,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, clap::ValueEnum, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Auto,
    Mam,
    Zlibrary,
}
impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Mam => "mam",
            Self::Zlibrary => "zlibrary",
        })
    }
}
pub fn remaining(deadline: Instant, cap: Duration) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .map(|d| d.min(cap))
        .ok_or_else(|| Error::dependency("Book search deadline"))
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Search {
        query: String,
        #[serde(default)]
        source: Source,
        author: Option<String>,
        language: Option<String>,
    },
    Select {
        release_id: String,
        language: Option<String>,
        idempotency_key: String,
    },
    Request {
        title: String,
        #[serde(default)]
        source: Source,
        author: Option<String>,
        language: Option<String>,
        idempotency_key: String,
    },
    Jobs {},
    Status {
        job_id: String,
    },
    Retry {
        job_id: String,
        #[serde(default)]
        acknowledge_possible_duplicate: bool,
    },
    ConfirmDelivery {
        job_id: String,
    },
}
fn cached_search(
    c: &Config,
    db: &Db,
    query: &str,
    author: Option<&str>,
    lang: Option<&str>,
    source: Source,
) -> Result<Value> {
    if query.trim().is_empty()
        || query.len() > 300
        || author.is_some_and(|a| a.trim().is_empty() || a.len() > 300)
    {
        return Err(Error::invalid(
            "Search title/query and optional author must contain 1–300 bytes",
        ));
    }
    let lang = lang.map(language).transpose()?;
    let start = Instant::now();
    let value = search_sources(
        source,
        query,
        author,
        lang.as_deref(),
        || integrations::search_until(c, query, start + Duration::from_secs(80)),
        || crate::zlibrary::search(c, query, start + Duration::from_secs(105)),
    )?;
    let releases: Vec<Release> = serde_json::from_value(value["releases"].clone())?;
    for r in releases {
        db.cache(&r)?;
    }
    Ok(value)
}

// Keep provider policy separate from transport so fallback decisions can be tested offline.
pub(crate) fn search_sources(
    source: Source,
    query: &str,
    author: Option<&str>,
    lang: Option<&str>,
    mam: impl FnOnce() -> Result<Value>,
    zlibrary: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    if source == Source::Zlibrary {
        let mut value = zlibrary()?;
        value["sources_searched"] = json!(["zlibrary"]);
        value["warnings"] = json!([]);
        return Ok(value);
    }
    let mut value = mam()?;
    value["sources_searched"] = json!(["mam"]);
    value["warnings"] = json!([]);
    let releases: Vec<Release> = serde_json::from_value(value["releases"].clone())?;
    if source == Source::Mam
        || releases
            .iter()
            .any(|r| selection::relevant(r, query, author, lang))
    {
        return Ok(value);
    }
    value["fallback_reason"] = json!("no_suitable_mam_match");
    match zlibrary() {
        Ok(z) => {
            let mut extra: Vec<Release> = serde_json::from_value(z["releases"].clone())?;
            let mut combined = releases;
            combined.append(&mut extra);
            value["releases"] = serde_json::to_value(combined)?;
            let total = value["reported_total"]
                .as_u64()
                .unwrap_or(0)
                .checked_add(z["reported_total"].as_u64().unwrap_or(0))
                .ok_or_else(|| {
                    Error::new(
                        "invalid_upstream_response",
                        "Combined provider result count exceeds its supported range",
                        4,
                    )
                })?;
            value["reported_total"] = json!(total);
            value["truncated"] = json!(value["truncated"] == true || z["truncated"] == true);
            value["sources_searched"] = json!(["mam", "zlibrary"]);
        }
        Err(e) => {
            eprintln!(
                "{}",
                json!({"event":"provider_search_failed","source":"zlibrary","error_code":e.code})
            );
            value["warnings"] = json!([{"source":"zlibrary","error":e}]);
        }
    }
    Ok(value)
}
fn submit(
    c: &Config,
    db: &mut Db,
    id: &str,
    lang: Option<&str>,
    key: &str,
    daily_limit: Option<u32>,
) -> Result<Value> {
    let release = db.release(id)?;
    let max_bytes = if release.source() == "zlibrary" {
        crate::zlibrary::configured(c)?;
        crate::zlibrary::MAX_EPUB_BYTES
    } else {
        c.max_torrent_bytes
    };
    if release.size_bytes > max_bytes {
        return Err(Error::new(
            "download_too_large",
            "Release exceeds the download size limit for its source",
            6,
        ));
    }
    if !release.eligible {
        return Err(Error::new(
            "release_restricted",
            format!("Release restrictions: {}", release.restrictions.join(", ")),
            5,
        ));
    }
    let l = lang
        .map(language)
        .transpose()?
        .or(release.language)
        .ok_or_else(|| Error::new("clarification_required", "Specify a release language", 3))?;
    let job = match daily_limit {
        Some(limit) => db.submit_limited(id, &l, key, Some(limit))?,
        None => db.submit(id, &l, key)?,
    };
    let job = resume_cdn_download(c, db, job)?;
    Ok(json!({"job":job}))
}

pub(crate) fn resume_cdn_download(c: &Config, db: &Db, job: Job) -> Result<Job> {
    let blocked = |j: &Job| {
        j.state == "failed" && j.release_id.starts_with("zlibrary:")
        && j.prepared_path.is_none() && j.error.as_ref().is_some_and(|e| {
            e["code"] == "unsafe_download_url" && e["message"].as_str()
                .and_then(|m| m.strip_prefix("Z-Library download host "))
                .and_then(|m| m.strip_suffix(" is not trusted; the owner must verify any additional download host"))
                .is_some_and(crate::zlibrary::approved_cdn)
        })
    };
    if !blocked(&job) {
        return Ok(job);
    }
    let tx = rusqlite::Transaction::new_unchecked(&db.0, rusqlite::TransactionBehavior::Immediate)?;
    let current = db.job(&job.id)?;
    let attempted: bool = db.0.query_row(
        "SELECT EXISTS(SELECT 1 FROM deliveries WHERE job_id=?1)",
        [&job.id],
        |r| r.get(0),
    )?;
    // Older releases rejected these CDN variants before any file bytes or email attempt.
    // Only an explicit selection/request can resume that specific failure after this policy upgrade.
    let job = if blocked(&current) && !attempted {
        crate::zlibrary::reset_partial(c, &current.id)?;
        let resumed = db.retry(&current.id, false)?;
        eprintln!(
            "{}",
            json!({"event":"download_resumed_after_cdn_policy_update","job_id":current.id,"source":"zlibrary"})
        );
        resumed
    } else {
        current
    };
    tx.commit()?;
    Ok(job)
}
pub fn execute(c: &Config, action: Action) -> Result<Value> {
    execute_limited(c, action, None)
}
pub fn execute_limited(c: &Config, action: Action, daily_limit: Option<u32>) -> Result<Value> {
    let mut db = Db::open(c)?;
    match action {
        Action::Search {
            query,
            source,
            author,
            language,
        } => cached_search(
            c,
            &db,
            &query,
            author.as_deref(),
            language.as_deref(),
            source,
        ),
        Action::Select {
            release_id,
            language,
            idempotency_key,
        } => submit(
            c,
            &mut db,
            &release_id,
            language.as_deref(),
            &idempotency_key,
            daily_limit,
        ),
        Action::Request {
            title,
            source,
            author,
            language,
            idempotency_key,
        } => {
            let results = cached_search(
                c,
                &db,
                &title,
                author.as_deref(),
                language.as_deref(),
                source,
            )?;
            let releases = serde_json::from_value::<Vec<Release>>(results["releases"].clone())?;
            let mut selected =
                selection::choose(&releases, &title, author.as_deref(), language.as_deref())?;
            selected["sources_searched"] = results["sources_searched"].clone();
            selected["warnings"] = results["warnings"].clone();
            if results["warnings"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
                && selected["status"] != "selected"
            {
                selected["status"] = json!("clarification_required");
                selected["reason"] = json!("search_incomplete");
            }
            if selected["status"] == "selected" {
                let mut out = submit(
                    c,
                    &mut db,
                    selected["release_id"].as_str().unwrap(),
                    selected["language"].as_str(),
                    &idempotency_key,
                    daily_limit,
                )?;
                out["selection"] = selected;
                Ok(out)
            } else {
                Ok(selected)
            }
        }
        Action::Jobs {} => Ok(json!({"jobs":db.jobs()?})),
        Action::Status { job_id } => Ok(json!({"job":db.job(&job_id)?})),
        Action::Retry {
            job_id,
            acknowledge_possible_duplicate,
        } => {
            let tx = rusqlite::Transaction::new_unchecked(
                &db.0,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let job = db.job(&job_id)?;
            if job.state == "failed" && job.release_id.starts_with("zlibrary:") {
                crate::zlibrary::reset_partial(c, &job.id)?;
            }
            let job = db.retry(&job_id, acknowledge_possible_duplicate)?;
            tx.commit()?;
            Ok(json!({"job":job}))
        }
        Action::ConfirmDelivery { job_id } => Ok(json!({"job":db.delivered(&job_id)?})),
    }
}
