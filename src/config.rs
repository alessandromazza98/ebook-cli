use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub processing_dir: PathBuf,
    pub mam: Mam,
    pub qbittorrent: Qbit,
    pub smtp: Smtp,
    pub api_token: String,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default = "default_max_torrent_bytes")]
    pub max_torrent_bytes: u64,
}
fn default_max_torrent_bytes() -> u64 {
    100 * 1024 * 1024
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    pub client_token: Option<String>,
    pub requests_per_minute: u32,
    pub jobs_per_day: u32,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            client_token: None,
            requests_per_minute: 30,
            jobs_per_day: 10,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mam {
    pub base_url: String,
    pub session: String,
    pub vip: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Qbit {
    pub url: String,
    pub username: String,
    pub password: String,
    pub category: String,
    pub remote_dir: PathBuf,
    pub local_dir: PathBuf,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub tls: String,
    pub username: String,
    pub password: String,
    pub from: String,
    pub kindle: String,
}
impl Config {
    pub fn example() -> Self {
        Self {
            state_dir: "/srv/ebook-cli/state".into(),
            processing_dir: "/srv/ebook-cli/processing".into(),
            mam: Mam {
                base_url: "https://www.myanonamouse.net".into(),
                session: String::new(),
                vip: false,
            },
            qbittorrent: Qbit {
                url: "http://127.0.0.1:8080".into(),
                username: String::new(),
                password: String::new(),
                category: "ebook-cli".into(),
                remote_dir: "/downloads/books/ebook-cli".into(),
                local_dir: "/data/downloads/books/ebook-cli".into(),
            },
            smtp: Smtp {
                host: String::new(),
                port: 587,
                tls: "starttls".into(),
                username: String::new(),
                password: String::new(),
                from: String::new(),
                kindle: String::new(),
            },
            api_token: uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string(),
            api: ApiConfig::default(),
            max_torrent_bytes: default_max_torrent_bytes(),
        }
    }
    pub fn load(path: &Path) -> Result<Self> {
        let m = fs::metadata(path)?;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(Error::invalid(
                "Config contains secrets and must have mode 0600",
            ));
        }
        let c: Self = serde_json::from_slice(&fs::read(path)?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        if self.api.client_token.as_ref().is_some_and(|token| {
            token.len() < 32
                || token == &self.api_token
                || !token.bytes().all(|b| b.is_ascii_graphic())
        }) {
            return Err(Error::invalid(
                "API client token must be distinct from the owner token and contain 32+ visible ASCII characters",
            ));
        }
        if !(1..=600).contains(&self.api.requests_per_minute)
            || !(1..=100).contains(&self.api.jobs_per_day)
            || !(1..=1024 * 1024 * 1024).contains(&self.max_torrent_bytes)
        {
            return Err(Error::invalid(
                "API limits must be 1–600 requests/minute, 1–100 jobs/day, and 1 byte–1 GiB per torrent",
            ));
        }
        let q = &self.qbittorrent;
        if q.category != "ebook-cli" {
            return Err(Error::invalid(
                "qBittorrent category must be ebook-cli to isolate this workflow",
            ));
        }
        for p in [
            &self.state_dir,
            &self.processing_dir,
            &q.local_dir,
            &q.remote_dir,
        ] {
            if !p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(Error::invalid(
                    "Paths must be absolute without parent traversal",
                ));
            }
        }
        let paths = [&self.state_dir, &self.processing_dir, &q.local_dir];
        for (i, a) in paths.iter().enumerate() {
            for b in paths.iter().skip(i + 1) {
                if a.starts_with(b) || b.starts_with(a) {
                    return Err(Error::invalid(
                        "State, processing and download directories must not overlap",
                    ));
                }
            }
        }
        if q.remote_dir.file_name().and_then(|s| s.to_str()) != Some("ebook-cli")
            || q.local_dir.file_name().and_then(|s| s.to_str()) != Some("ebook-cli")
        {
            return Err(Error::invalid("Download paths must end in ebook-cli"));
        }
        let mam = reqwest::Url::parse(&self.mam.base_url)
            .map_err(|_| Error::invalid("Invalid MAM URL"))?;
        if mam.scheme() != "https"
            || !matches!(
                mam.host_str(),
                Some("www.myanonamouse.net" | "myanonamouse.net")
            )
            || mam.path() != "/"
        {
            return Err(Error::invalid("MAM must use its official HTTPS origin"));
        }
        let qb =
            reqwest::Url::parse(&q.url).map_err(|_| Error::invalid("Invalid qBittorrent URL"))?;
        if !matches!(qb.scheme(), "http" | "https")
            || !qb.username().is_empty()
            || qb.password().is_some()
            || qb.query().is_some()
        {
            return Err(Error::invalid("Invalid qBittorrent origin"));
        }
        if self.mam.session.is_empty() || self.mam.session.contains([';', '\r', '\n']) {
            return Err(Error::invalid("Configure a valid MAM session"));
        }
        if !matches!(self.smtp.tls.as_str(), "starttls" | "tls") {
            return Err(Error::invalid("SMTP must use starttls or tls"));
        }
        for a in [&self.smtp.from, &self.smtp.kindle] {
            a.parse::<lettre::message::Mailbox>()
                .map_err(|_| Error::invalid("Configure valid sender and Kindle mailboxes"))?;
        }
        if self.smtp.host.is_empty() || self.api_token.len() < 32 {
            return Err(Error::invalid(
                "SMTP host and API token (32+ characters) are required",
            ));
        }
        Ok(())
    }
    pub fn init_dirs(&self) -> Result<()> {
        for d in [&self.state_dir, &self.processing_dir] {
            fs::create_dir_all(d)?;
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    pub fn setup(path: &Path) -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        f.write_all(&serde_json::to_vec_pretty(&Self::example())?)?;
        f.sync_all()?;
        Ok(())
    }
}

pub fn language(input: &str) -> Result<String> {
    let l = input.trim().to_lowercase();
    let mapped = match l.as_str() {
        "english" | "eng" => "en",
        "french" | "fra" | "fre" => "fr",
        "german" | "deu" | "ger" => "de",
        "spanish" | "spa" => "es",
        "italian" | "ita" => "it",
        "portuguese" | "por" => "pt",
        "dutch" | "nld" => "nl",
        "russian" | "rus" => "ru",
        "greek" | "ell" => "el",
        "romanian" | "ron" | "rom" => "ro",
        "polish" | "pol" => "pl",
        "japanese" | "jpn" => "ja",
        "chinese" | "zho" => "zh",
        _ => &l,
    };
    if !regex::Regex::new(r"^[a-z]{2,3}(-[a-z0-9]{2,8})*$")
        .unwrap()
        .is_match(mapped)
    {
        return Err(Error::invalid(
            "Language must be an ISO/BCP-47 tag or a supported language name",
        ));
    }
    Ok(mapped.into())
}
