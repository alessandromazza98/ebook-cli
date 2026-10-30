use crate::{
    config::{Config, language},
    error::{Error, Result},
};
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    pub author: String,
    pub language: Option<String>,
    pub format: String,
    pub size_bytes: u64,
    pub seeders: u64,
    pub restrictions: Vec<String>,
    pub eligible: bool,
}
pub fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .user_agent("ebook-cli/0.1")
        .build()
        .map_err(|_| Error::dependency("HTTP client initialization"))
}
pub fn bytes(r: Response, limit: u64, op: &str) -> Result<Vec<u8>> {
    if !r.status().is_success() {
        return Err(Error::dependency(op));
    }
    let mut b = Vec::new();
    r.take(limit + 1)
        .read_to_end(&mut b)
        .map_err(|_| Error::dependency(op))?;
    if b.len() as u64 > limit {
        return Err(Error::new(
            "upstream_response_too_large",
            format!("{op}: response exceeds limit"),
            4,
        ));
    }
    Ok(b)
}
fn acknowledgement(r: Response, op: &str) -> Result<()> {
    let status = r.status();
    let body = bytes(r, 1024, op)?;
    if (status == reqwest::StatusCode::OK && body == b"Ok.")
        || (status == reqwest::StatusCode::NO_CONTENT && body.is_empty())
    {
        Ok(())
    } else {
        Err(Error::dependency(op))
    }
}
pub fn add_acknowledged(status: reqwest::StatusCode, body: &[u8], hash: &str) -> bool {
    if (status == reqwest::StatusCode::OK && body == b"Ok.")
        || (status == reqwest::StatusCode::NO_CONTENT && body.is_empty())
    {
        return true;
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    status == reqwest::StatusCode::OK
        && v["success_count"] == 1
        && v["failure_count"] == 0
        && v["pending_count"] == 0
        && v["added_torrent_ids"]
            .as_array()
            .is_some_and(|ids| ids.len() == 1 && ids[0].as_str() == Some(hash))
}
fn txt(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
fn flag(v: &Value, k: &str) -> bool {
    v[k] == true || v[k] == 1 || v[k] == "1"
}
fn number(v: &Value, k: &str) -> Result<u64> {
    v[k].as_u64()
        .or_else(|| v[k].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| {
            Error::new(
                "invalid_upstream_response",
                format!("MAM {k} must be a non-negative integer"),
                4,
            )
        })
}
fn size(s: &str) -> Result<u64> {
    let clean = s.replace(',', "");
    if let Ok(n) = clean.parse() {
        return Ok(n);
    }
    let parts: Vec<_> = clean.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(Error::new(
            "invalid_upstream_response",
            "Invalid MAM size",
            4,
        ));
    }
    let n: f64 = parts[0]
        .parse()
        .map_err(|_| Error::new("invalid_upstream_response", "Invalid MAM size", 4))?;
    let mult = match parts[1].to_uppercase().as_str() {
        "B" => 1.,
        "KB" | "KIB" => 1024.,
        "MB" | "MIB" => 1048576.,
        "GB" | "GIB" => 1073741824.,
        _ => {
            return Err(Error::new(
                "invalid_upstream_response",
                "Unknown MAM size unit",
                4,
            ));
        }
    };
    if !n.is_finite() || n < 0. || n * mult > u64::MAX as f64 {
        return Err(Error::new(
            "invalid_upstream_response",
            "Invalid MAM size range",
            4,
        ));
    }
    Ok((n * mult) as u64)
}
pub fn parse_release(v: &Value, vip: bool) -> Result<Release> {
    let id = txt(v, "id");
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Error::new(
            "invalid_upstream_response",
            "Invalid MAM release ID",
            4,
        ));
    }
    let title = txt(v, "title");
    if title.is_empty() {
        return Err(Error::new(
            "invalid_upstream_response",
            "MAM title is missing",
            4,
        ));
    }
    let a = &v["author_info"];
    let authors: Value = if let Some(s) = a.as_str() {
        serde_json::from_str(s).map_err(|_| {
            Error::new(
                "invalid_upstream_response",
                "Malformed MAM author metadata",
                4,
            )
        })?
    } else {
        a.clone()
    };
    let author = authors
        .as_object()
        .map(|o| {
            o.values()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let format = txt(v, "filetype").to_uppercase();
    let mut restrictions = vec![];
    if flag(v, "vip") && !vip {
        restrictions.push("vip_required".into());
    }
    if !format
        .split(|c: char| !c.is_alphanumeric())
        .any(|x| x == "EPUB")
    {
        restrictions.push("no_epub".into());
    }
    let seeders = number(v, "seeders")?;
    if seeders == 0 {
        restrictions.push("no_seeders".into());
    }
    let language = if txt(v, "lang_code").is_empty() {
        None
    } else {
        language(&txt(v, "lang_code")).ok()
    };
    Ok(Release {
        id: format!("mam:{id}"),
        title,
        author,
        language,
        format,
        size_bytes: size(&txt(v, "size"))?,
        seeders,
        eligible: restrictions.is_empty(),
        restrictions,
    })
}
fn cookie(c: &Config) -> String {
    format!("mam_id={}; mam_ssl=1", c.mam.session)
}
pub fn search(c: &Config, query: &str) -> Result<Value> {
    if query.trim().is_empty() || query.len() > 300 {
        return Err(Error::invalid("Search query must contain 1–300 bytes"));
    }
    let http = client()?;
    let mut releases = Vec::new();
    let mut total = 0;
    for page in 0..3 {
        let body = json!({"tor":{"text":query,"srchIn":["title","author"],"searchType":"all","searchIn":"torrents","cat":["0"],"main_cat":["14"],"sortType":"default","startNumber":(page*100).to_string()},"perpage":"100","mediaInfo":"1"});
        let r = http
            .post(format!(
                "{}/tor/js/loadSearchJSONbasic.php",
                c.mam.base_url.trim_end_matches('/')
            ))
            .header("Cookie", cookie(c))
            .json(&body)
            .send()
            .map_err(|_| Error::dependency("MAM search"))?;
        let data: Value = serde_json::from_slice(&bytes(r, 4 * 1024 * 1024, "MAM search")?)
            .map_err(|_| {
                Error::new(
                    "invalid_upstream_response",
                    "MAM returned non-JSON; session may have expired",
                    4,
                )
            })?;
        let rows = data["data"].as_array().ok_or_else(|| {
            Error::new(
                "invalid_upstream_response",
                "MAM search data is missing; check session/account",
                4,
            )
        })?;
        total = data["found"]
            .as_u64()
            .or_else(|| data["found"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(rows.len() as u64);
        for row in rows {
            releases.push(parse_release(
                if row.is_array() { &row[0] } else { row },
                c.mam.vip,
            )?);
        }
        if rows.len() < 100 || releases.len() as u64 >= total {
            break;
        }
    }
    Ok(json!({"releases":releases,"reported_total":total,"truncated":total>releases.len() as u64}))
}
pub fn torrent(c: &Config, id: &str) -> Result<Vec<u8>> {
    let id = id
        .strip_prefix("mam:")
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| Error::invalid("Expected mam:<numeric ID>"))?;
    let r = client()?
        .get(format!(
            "{}/tor/download.php",
            c.mam.base_url.trim_end_matches('/')
        ))
        .query(&[("tid", id)])
        .header("Cookie", cookie(c))
        .send()
        .map_err(|_| Error::dependency("MAM torrent download"))?;
    bytes(r, 4 * 1024 * 1024, "MAM torrent download")
}

pub struct Qb<'a> {
    http: Client,
    c: &'a Config,
}
impl<'a> Qb<'a> {
    pub fn connect(c: &'a Config) -> Result<Self> {
        let http = client()?;
        let r = http
            .post(format!(
                "{}/api/v2/auth/login",
                c.qbittorrent.url.trim_end_matches('/')
            ))
            .form(&[
                ("username", &c.qbittorrent.username),
                ("password", &c.qbittorrent.password),
            ])
            .send()
            .map_err(|_| Error::dependency("qBittorrent login"))?;
        acknowledgement(r, "qBittorrent authentication")?;
        Ok(Self { http, c })
    }
    pub fn get(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<Value> {
        let r = self
            .http
            .get(format!(
                "{}/api/v2/{endpoint}",
                self.c.qbittorrent.url.trim_end_matches('/')
            ))
            .query(query)
            .send()
            .map_err(|_| Error::dependency(endpoint))?;
        serde_json::from_slice(&bytes(r, 4 * 1024 * 1024, endpoint)?).map_err(|_| {
            Error::new(
                "invalid_upstream_response",
                format!("qBittorrent {endpoint} returned invalid JSON"),
                4,
            )
        })
    }
    pub fn category(&self, create: bool) -> Result<()> {
        let cats = self.get("torrents/categories", &[])?;
        let q = &self.c.qbittorrent;
        if let Some(cat) = cats.get(&q.category) {
            if cat["savePath"].as_str() != q.remote_dir.to_str() {
                return Err(Error::conflict(
                    "Existing ebook-cli category has a different save path",
                ));
            }
        } else if create {
            let r = self
                .http
                .post(format!(
                    "{}/api/v2/torrents/createCategory",
                    q.url.trim_end_matches('/')
                ))
                .form(&[
                    ("category", q.category.as_str()),
                    ("savePath", q.remote_dir.to_str().unwrap()),
                ])
                .send()
                .map_err(|_| Error::dependency("qBittorrent category creation"))?;
            bytes(r, 1024, "qBittorrent category creation")?;
        } else {
            return Err(Error::conflict(
                "Run ebook check --create-category to create the isolated category",
            ));
        }
        Ok(())
    }
    pub fn info(&self, hash: &str) -> Result<Option<Value>> {
        let v = self.get("torrents/info", &[("hashes", hash)])?;
        let a = v.as_array().ok_or_else(|| {
            Error::new(
                "invalid_upstream_response",
                "qBittorrent torrent list must be an array",
                4,
            )
        })?;
        if a.len() > 1 {
            return Err(Error::new(
                "invalid_upstream_response",
                "qBittorrent hash lookup returned multiple torrents",
                4,
            ));
        }
        Ok(a.first().cloned())
    }
    pub fn owned(&self, v: &Value) -> Result<()> {
        if v["category"].as_str() != Some(&self.c.qbittorrent.category) {
            return Err(Error::new(
                "existing_workflow",
                "Selected torrent already belongs to another workflow; its category was preserved",
                5,
            ));
        }
        if v["save_path"].as_str().map(std::path::Path::new)
            != Some(self.c.qbittorrent.remote_dir.as_path())
        {
            return Err(Error::conflict(
                "Torrent is outside the CLI download directory",
            ));
        }
        Ok(())
    }
    pub fn add(&self, data: Vec<u8>, hash: &str) -> Result<()> {
        let q = &self.c.qbittorrent;
        let form = reqwest::blocking::multipart::Form::new()
            .part(
                "torrents",
                reqwest::blocking::multipart::Part::bytes(data).file_name("book.torrent"),
            )
            .text("category", q.category.clone())
            .text("savepath", q.remote_dir.to_string_lossy().to_string())
            .text("autoTMM", "false")
            .text("skip_checking", "false")
            .text("ratioLimit", "-1")
            .text("seedingTimeLimit", "-1")
            .text("inactiveSeedingTimeLimit", "-1");
        let r = self
            .http
            .post(format!(
                "{}/api/v2/torrents/add",
                q.url.trim_end_matches('/')
            ))
            .multipart(form)
            .send()
            .map_err(|_| Error::dependency("qBittorrent torrent add"))?;
        let status = r.status();
        let body = bytes(r, 4096, "qBittorrent torrent add")?;
        if !add_acknowledged(status, &body, hash) {
            return Err(Error::dependency(
                "qBittorrent torrent add acknowledgement; next tick reconciles the saved hash",
            ));
        }
        Ok(())
    }
}
