use crate::{
    config::{Config, language},
    db::Db,
    error::{Error, Result},
    integrations::{self, Release},
    selection,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Search {
        query: String,
    },
    Select {
        release_id: String,
        language: Option<String>,
        idempotency_key: String,
    },
    Request {
        title: String,
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
fn cached_search(c: &Config, db: &Db, query: &str) -> Result<Value> {
    let value = integrations::search(c, query)?;
    let releases: Vec<Release> = serde_json::from_value(value["releases"].clone())?;
    for r in releases {
        db.cache(&r)?;
    }
    Ok(value)
}
fn submit(db: &mut Db, id: &str, lang: Option<&str>, key: &str) -> Result<Value> {
    let release = db.release(id)?;
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
    Ok(json!({"job":db.submit(id,&l,key)?}))
}
pub fn execute(c: &Config, action: Action) -> Result<Value> {
    let mut db = Db::open(c)?;
    match action {
        Action::Search { query } => cached_search(c, &db, &query),
        Action::Select {
            release_id,
            language,
            idempotency_key,
        } => submit(&mut db, &release_id, language.as_deref(), &idempotency_key),
        Action::Request {
            title,
            author,
            language,
            idempotency_key,
        } => {
            let results = cached_search(c, &db, &title)?;
            let releases = serde_json::from_value::<Vec<Release>>(results["releases"].clone())?;
            let selected =
                selection::choose(&releases, &title, author.as_deref(), language.as_deref())?;
            if selected["status"] == "selected" {
                let mut out = submit(
                    &mut db,
                    selected["release_id"].as_str().unwrap(),
                    selected["language"].as_str(),
                    &idempotency_key,
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
        } => Ok(json!({"job":db.retry(&job_id,acknowledge_possible_duplicate)?})),
        Action::ConfirmDelivery { job_id } => Ok(json!({"job":db.delivered(&job_id)?})),
    }
}
