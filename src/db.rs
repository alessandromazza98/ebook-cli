use crate::{
    config::Config,
    error::{Error, Result},
    integrations::Release,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub struct Db(pub Connection);
#[derive(Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub release_id: String,
    pub language: String,
    pub state: String,
    pub torrent_hash: Option<String>,
    pub prepared_path: Option<String>,
    pub message_id: String,
    pub error: Option<serde_json::Value>,
    pub updated_at: i64,
}
impl Db {
    pub fn open(c: &Config) -> Result<Self> {
        c.init_dirs()?;
        let conn = Connection::open(c.state_dir.join("jobs.sqlite"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
            return Err(Error::new(
                "incompatible_database",
                "Job database was created by a newer CLI; use a compatible binary",
                7,
            ));
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS releases(id TEXT PRIMARY KEY, payload TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS jobs(
            id TEXT PRIMARY KEY, release_id TEXT NOT NULL UNIQUE, language TEXT NOT NULL,
            state TEXT NOT NULL, torrent_hash TEXT UNIQUE, prepared_path TEXT,
            message_id TEXT NOT NULL UNIQUE, error TEXT, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
          CREATE TABLE IF NOT EXISTS submissions(key TEXT PRIMARY KEY, payload TEXT NOT NULL, job_id TEXT NOT NULL REFERENCES jobs(id));
          CREATE TABLE IF NOT EXISTS deliveries(digest TEXT PRIMARY KEY, job_id TEXT NOT NULL UNIQUE REFERENCES jobs(id));
          CREATE TABLE IF NOT EXISTS failures(job_id TEXT PRIMARY KEY REFERENCES jobs(id), count INTEGER NOT NULL);
          PRAGMA user_version=1;")?;
        Ok(Self(conn))
    }
    pub fn cache(&self, r: &Release) -> Result<()> {
        self.0.execute("INSERT INTO releases VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![r.id, serde_json::to_string(r)?])?;
        Ok(())
    }
    pub fn release(&self, id: &str) -> Result<Release> {
        let s: Option<String> = self
            .0
            .query_row("SELECT payload FROM releases WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        serde_json::from_str(
            &s.ok_or_else(|| Error::new("not_found", "Release not cached; search first", 3))?,
        )
        .map_err(Into::into)
    }
    pub fn submit(&mut self, release: &str, lang: &str, key: &str) -> Result<Job> {
        if key.is_empty() || key.len() > 128 {
            return Err(Error::invalid(
                "Idempotency key must contain 1–128 characters",
            ));
        }
        let payload = serde_json::to_string(&(release, lang))?;
        let tx = self
            .0
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: Option<(String, String)> = tx
            .query_row(
                "SELECT payload,job_id FROM submissions WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((p, id)) = previous {
            if p != payload {
                return Err(Error::conflict(
                    "Idempotency key already used for a different submission",
                ));
            }
            tx.commit()?;
            return self.job(&id);
        }
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id,language FROM jobs WHERE release_id=?1",
                [release],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id = if let Some((id, old_lang)) = existing {
            if old_lang != lang {
                return Err(Error::conflict(
                    "Release already requested in another language",
                ));
            }
            id
        } else {
            let active:i64=tx.query_row("SELECT count(*) FROM jobs WHERE state IN ('queued','downloading','preparing','ready','sending')",[],|r|r.get(0))?;
            if active >= 20 {
                return Err(Error::conflict(
                    "Queue contains 20 active jobs; wait for completion before submitting more",
                ));
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO jobs(id,release_id,language,state,message_id) VALUES(?1,?2,?3,'queued',?4)", params![id,release,lang,format!("<{id}@ebook-cli.local>")])?;
            id
        };
        tx.execute(
            "INSERT INTO submissions VALUES(?1,?2,?3)",
            params![key, payload, id],
        )?;
        tx.commit()?;
        self.job(&id)
    }
    pub fn job(&self, id: &str) -> Result<Job> {
        self.jobs()?
            .into_iter()
            .find(|j| j.id == id)
            .ok_or_else(|| Error::new("not_found", "Unknown job ID", 3))
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        let mut s = self.0.prepare("SELECT id,release_id,language,state,torrent_hash,prepared_path,message_id,error,updated_at FROM jobs ORDER BY updated_at,id")?;
        let rows = s.query_map([], |r| {
            Ok((
                Job {
                    id: r.get(0)?,
                    release_id: r.get(1)?,
                    language: r.get(2)?,
                    state: r.get(3)?,
                    torrent_hash: r.get(4)?,
                    prepared_path: r.get(5)?,
                    message_id: r.get(6)?,
                    error: None,
                    updated_at: r.get(8)?,
                },
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (mut j, e) = row?;
            j.error = e.map(|s| serde_json::from_str(&s)).transpose()?;
            out.push(j);
        }
        Ok(out)
    }
    pub fn state(&self, id: &str, state: &str, error: Option<&Error>) -> Result<()> {
        let e = error.map(serde_json::to_string).transpose()?;
        self.0.execute(
            "UPDATE jobs SET state=?2,error=?3,updated_at=unixepoch() WHERE id=?1",
            params![id, state, e],
        )?;
        Ok(())
    }
    pub fn hash(&self, id: &str, hash: &str) -> Result<()> {
        let owner: Option<String> = self
            .0
            .query_row(
                "SELECT id FROM jobs WHERE torrent_hash=?1 AND id!=?2",
                params![hash, id],
                |r| r.get(0),
            )
            .optional()?;
        if owner.is_some() {
            return Err(Error::conflict(
                "Torrent hash already belongs to another CLI job",
            ));
        }
        self.0.execute(
            "UPDATE jobs SET torrent_hash=?2 WHERE id=?1",
            params![id, hash],
        )?;
        Ok(())
    }
    pub fn prepared(&self, id: &str, path: &str) -> Result<()> {
        self.0.execute("UPDATE jobs SET prepared_path=?2,state='ready',error=NULL,updated_at=unixepoch() WHERE id=?1",params![id,path])?;
        Ok(())
    }
    pub fn recover(&self) -> Result<()> {
        let error = serde_json::to_string(&Error::new(
            "send_uncertain",
            "Worker stopped during SMTP; verify delivery before explicitly retrying",
            5,
        ))?;
        self.0.execute("UPDATE jobs SET state='send_uncertain',error=?1,updated_at=unixepoch() WHERE state='sending'",[error])?;
        Ok(())
    }
    pub fn retry(&self, id: &str, ack: bool) -> Result<Job> {
        let j = self.job(id)?;
        let next = match j.state.as_str() {
            "failed" => "queued",
            "send_uncertain" if ack => "ready",
            "send_uncertain" => {
                return Err(Error::conflict(
                    "Verify Kindle/email first, then use --acknowledge-possible-duplicate",
                ));
            }
            _ => {
                return Err(Error::conflict(
                    "Only failed or uncertain jobs can be retried",
                ));
            }
        };
        self.transition_terminal(id, &j.state, next)?;
        self.job(id)
    }
    pub fn delivered(&self, id: &str) -> Result<Job> {
        let j = self.job(id)?;
        if !matches!(j.state.as_str(), "smtp_accepted" | "send_uncertain") {
            return Err(Error::conflict("Job has no send attempt to confirm"));
        }
        self.transition_terminal(id, &j.state, "delivered")?;
        self.job(id)
    }
    fn transition_terminal(&self, id: &str, previous: &str, next: &str) -> Result<()> {
        let changed = self.0.execute(
            "UPDATE jobs SET state=?3,error=NULL,updated_at=unixepoch() WHERE id=?1 AND state=?2",
            params![id, previous, next],
        )?;
        if changed != 1 {
            return Err(Error::conflict(
                "Job changed concurrently; read its current status before retrying",
            ));
        }
        Ok(())
    }
}
