use crate::{
    config::Config,
    db::{Db, Job},
    epub,
    error::{Error, Result},
    integrations::Qb,
    torrent,
};
use fs2::FileExt;
use lettre::{
    Message, SmtpTransport, Transport,
    message::{Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path},
    time::Duration,
};

pub fn lock(c: &Config) -> Result<fs::File> {
    c.init_dirs()?;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(c.state_dir.join("worker.lock"))?;
    f.try_lock_exclusive().map_err(|_| {
        Error::conflict("Another worker or job transition currently holds the worker lock")
    })?;
    Ok(f)
}
fn digest(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
pub fn smtp(c: &Config) -> Result<SmtpTransport> {
    let builder = match c.smtp.tls.as_str() {
        "tls" => SmtpTransport::relay(&c.smtp.host),
        "starttls" => SmtpTransport::starttls_relay(&c.smtp.host),
        _ => return Err(Error::invalid("SMTP TLS is required")),
    }
    .map_err(|_| Error::invalid("Invalid SMTP host"))?;
    let builder = builder
        .port(c.smtp.port)
        .timeout(Some(Duration::from_secs(30)));
    let builder = if c.smtp.username.is_empty() {
        builder
    } else {
        builder.credentials(Credentials::new(
            c.smtp.username.clone(),
            c.smtp.password.clone(),
        ))
    };
    Ok(builder.build())
}
fn message(c: &Config, j: &Job, path: &Path) -> Result<Message> {
    if fs::metadata(path)?.len() > 30 * 1024 * 1024 {
        return Err(Error::new(
            "unsafe_epub",
            "Prepared attachment too large",
            6,
        ));
    }
    Message::builder()
        .from(
            c.smtp
                .from
                .parse()
                .map_err(|_| Error::invalid("Invalid sender"))?,
        )
        .to(c
            .smtp
            .kindle
            .parse()
            .map_err(|_| Error::invalid("Invalid Kindle address"))?)
        .message_id(Some(j.message_id.clone()))
        .subject("Ebook delivery")
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain("Requested ebook attached.".to_string()))
                .singlepart(
                    Attachment::new("book.epub".into())
                        .body(fs::read(path)?, "application/epub+zip".parse().unwrap()),
                ),
        )
        .map_err(|_| Error::invalid("Cannot construct email message"))
}
fn step(c: &Config, db: &Db, j: &Job) -> Result<()> {
    match j.state.as_str() {
        "queued" => {
            let qb = Qb::connect(c)?;
            qb.category(false)?;
            let path = c.state_dir.join(format!("{}.torrent", j.id));
            let data = if path.exists() {
                fs::read(&path)?
            } else {
                let data = crate::integrations::torrent(c, &j.release_id)?;
                torrent::info_hash(&data)?;
                let tmp = path.with_extension("torrent.tmp");
                fs::write(&tmp, &data)?;
                fs::File::open(&tmp)?.sync_all()?;
                fs::rename(tmp, &path)?;
                fs::File::open(&c.state_dir)?.sync_all()?;
                data
            };
            let hash = torrent::info_hash(&data)?;
            torrent::validate_download(&data, c.max_torrent_bytes)?;
            db.hash(&j.id, &hash)?;
            if let Some(existing) = qb.info(&hash)? {
                qb.owned(&existing)?;
            } else {
                qb.add(data, &hash)?;
            }
            // Retry reconciles the same info hash after a lost add response.
            db.state(&j.id, "downloading", None)?;
        }
        "downloading" => {
            let hash = j
                .torrent_hash
                .as_deref()
                .ok_or_else(|| Error::conflict("Job is missing its torrent hash"))?;
            let qb = Qb::connect(c)?;
            let info = qb.info(hash)?.ok_or_else(|| {
                Error::conflict("CLI torrent disappeared; retry explicitly to re-add")
            })?;
            qb.owned(&info)?;
            let state = info["state"].as_str().ok_or_else(|| {
                Error::new("invalid_upstream_response", "Missing torrent state", 4)
            })?;
            if matches!(state, "error" | "missingFiles") {
                return Err(Error::conflict(
                    "qBittorrent reports missing files or a download error",
                ));
            }
            if info["progress"].as_f64().unwrap_or(0.) < 1.
                || info["amount_left"].as_u64() != Some(0)
                || state.contains("checking")
                || state.contains("moving")
            {
                return Ok(());
            }
            let files = qb.get("torrents/files", &[("hash", hash)])?;
            let entries = files.as_array().ok_or_else(|| {
                Error::new("invalid_upstream_response", "Missing torrent file list", 4)
            })?;
            let epubs: Vec<_> = entries
                .iter()
                .filter(|e| {
                    e["name"]
                        .as_str()
                        .is_some_and(|n| n.to_lowercase().ends_with(".epub"))
                })
                .collect();
            if epubs.len() != 1 {
                return Err(Error::new(
                    "epub_selection_required",
                    "Torrent must contain exactly one EPUB",
                    6,
                ));
            }
            let entry = epubs[0];
            if entry["progress"].as_f64() != Some(1.)
                || entry["size"].as_u64().is_none_or(|s| s > 30 * 1024 * 1024)
            {
                return Err(Error::new("unsafe_epub", "Incomplete or oversized EPUB", 6));
            }
            let name = Path::new(entry["name"].as_str().unwrap());
            if name
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
                || name.to_string_lossy().contains('\\')
            {
                return Err(Error::new("unsafe_epub", "Unsafe torrent file path", 6));
            }
            let root = c.qbittorrent.local_dir.canonicalize()?;
            let source = root.join(name).canonicalize()?;
            if !source.starts_with(&root) || !source.is_file() {
                return Err(Error::new(
                    "unsafe_epub",
                    "Torrent path escapes download directory",
                    6,
                ));
            }
            let dir = c.processing_dir.join(&j.id);
            fs::create_dir_all(&dir)?;
            fs::copy(&source, dir.join("source.epub"))?;
            db.state(&j.id, "preparing", None)?;
        }
        "preparing" => {
            let dir = c.processing_dir.join(&j.id);
            let tmp = dir.join(format!("{}.tmp.epub", uuid::Uuid::new_v4()));
            let dest = dir.join("prepared.epub");
            let report = epub::prepare(&dir.join("source.epub"), &tmp, &j.language)?;
            fs::rename(tmp, &dest)?;
            fs::write(dir.join("repair.json"), serde_json::to_vec_pretty(&report)?)?;
            fs::File::open(&dir)?.sync_all()?;
            db.prepared(
                &j.id,
                dest.to_str()
                    .ok_or_else(|| Error::invalid("Non-UTF8 processing path"))?,
            )?;
        }
        "ready" => {
            let path = Path::new(
                j.prepared_path
                    .as_deref()
                    .ok_or_else(|| Error::conflict("Missing prepared attachment"))?,
            );
            let mail = message(c, j, path)?;
            let transport = smtp(c)?;
            let hash = digest(path)?;
            let prior: Option<String> =
                db.0.query_row(
                    "SELECT job_id FROM deliveries WHERE digest=?1",
                    [&hash],
                    |r| r.get(0),
                )
                .optional()?;
            if prior.as_ref().is_some_and(|id| id != &j.id) {
                return Err(Error::conflict(
                    "Identical prepared EPUB already has a delivery attempt; inspect the earlier job",
                ));
            }
            db.0.execute(
                "INSERT OR IGNORE INTO deliveries VALUES(?1,?2)",
                rusqlite::params![hash, j.id],
            )?;
            db.state(&j.id, "sending", None)?;
            match transport.send(&mail){
                Ok(_)=>db.state(&j.id,"smtp_accepted",None)?,
                Err(_)=>db.state(&j.id,"send_uncertain",Some(&Error::new("send_uncertain","SMTP failed or acceptance is unknown; inspect Kindle/email before retrying",5)))?,
            }
        }
        _ => {}
    }
    Ok(())
}
use rusqlite::OptionalExtension;
pub fn tick(c: &Config, db: &Db) -> Result<serde_json::Value> {
    let mut events = Vec::new();
    for j in db
        .jobs()?
        .into_iter()
        .filter(|j| {
            matches!(
                j.state.as_str(),
                "queued" | "downloading" | "preparing" | "ready"
            )
        })
        .take(10)
    {
        match step(c, db, &j) {
            Ok(()) => {
                db.0.execute("DELETE FROM failures WHERE job_id=?1", [&j.id])?;
                db.0.execute(
                    "UPDATE jobs SET updated_at=unixepoch() WHERE id=?1",
                    [&j.id],
                )?;
            }
            Err(e) => {
                if db.job(&j.id)?.state == "sending" {
                    db.state(
                        &j.id,
                        "send_uncertain",
                        Some(&Error::new(
                            "send_uncertain",
                            "Send attempt interrupted; inspect delivery before retrying",
                            5,
                        )),
                    )?;
                    events.push(json!({"job_id":j.id,"state":"send_uncertain"}));
                    continue;
                }
                db.0.execute("INSERT INTO failures VALUES(?1,1) ON CONFLICT(job_id) DO UPDATE SET count=count+1",[&j.id])?;
                let count: i64 =
                    db.0.query_row("SELECT count FROM failures WHERE job_id=?1", [&j.id], |r| {
                        r.get(0)
                    })?;
                let state = if e.code == "existing_workflow" {
                    "existing_workflow"
                } else if e.retryable && count < 5 {
                    &j.state
                } else {
                    "failed"
                };
                db.state(&j.id, state, Some(&e))?;
            }
        }
        let after = db.job(&j.id)?;
        if after.state != j.state || after.error.is_some() {
            events.push(json!({"job_id":j.id,"state":after.state,"error":after.error}));
        }
    }
    Ok(json!({"events":events}))
}
