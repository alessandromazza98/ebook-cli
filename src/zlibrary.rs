//! Guest-only Z-Library EAPI metadata and website downloads. No account credentials are accepted.
use crate::{
    config::{Config, Zlibrary, language},
    core::remaining,
    error::{Error, Result},
    integrations::Release,
};
use reqwest::{
    Url,
    blocking::{Client, RequestBuilder, Response},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_EPUB_BYTES: u64 = 30 * 1024 * 1024;

pub fn origin(input: &str) -> Result<Url> {
    let url = Url::parse(input).map_err(|_| Error::invalid("Invalid Z-Library HTTPS origin"))?;
    if url.scheme() != "https"
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !url.host_str().is_some_and(public_hostname)
    {
        return Err(Error::invalid(
            "Z-Library requires a public HTTPS DNS origin without credentials, port, path or query",
        ));
    }
    Ok(url)
}
fn public_hostname(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_err()
        && host.contains('.')
        && !host.ends_with('.')
        && !host.ends_with(".localhost")
        && !host.ends_with(".local")
        && !host.ends_with(".internal")
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}
pub fn configured(c: &Config) -> Result<&Zlibrary> {
    c.zlibrary.as_ref().filter(|z| z.enabled).ok_or_else(|| {
        Error::new(
            "zlibrary_not_configured",
            "Z-Library guest access is disabled; the owner must configure a trusted HTTPS base_url",
            5,
        )
    })
}
fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .user_agent(concat!("ebook-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Error::dependency("Z-Library HTTP client"))
}
fn guest_blocked() -> Error {
    Error::new(
        "zlibrary_guest_blocked",
        "Z-Library is requiring a browser check; guest downloads are unavailable from this server. No file was sent",
        4,
    )
}
fn quota() -> Error {
    Error::new(
        "zlibrary_quota_exceeded",
        "Z-Library's guest/IP download quota is exhausted; wait for its reset before an owner retry",
        4,
    )
}
fn malformed() -> Error {
    Error::new(
        "invalid_upstream_response",
        "Z-Library returned malformed or incompatible guest metadata",
        4,
    )
}
fn unavailable(operation: &str) -> Error {
    let mut error = Error::new(
        "dependency_unavailable",
        format!("{operation} failed; check connectivity and Z-Library availability"),
        4,
    );
    error.retryable = true;
    error
}
pub(crate) fn decode(status: reqwest::StatusCode, body: &[u8]) -> Result<Value> {
    let parsed = serde_json::from_slice::<Value>(body);
    if status.is_success()
        && parsed
            .as_ref()
            .is_ok_and(|v| v["success"] == 1 || v["success"] == true)
    {
        return parsed.map_err(|_| malformed());
    }
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(quota());
    }
    if (lower.contains("<html") || lower.contains("<!doctype html"))
        && (lower.contains("download limit")
            || lower.contains("daily limit")
            || lower.contains("download quota")
            || lower.contains("you have downloaded"))
    {
        return Err(quota());
    }
    if lower.contains("checking your browser")
        || lower.contains("diamwall")
        || lower.contains("<title>just a moment")
        || lower.contains("cf-chl-")
        || status.is_redirection()
    {
        return Err(guest_blocked());
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(Error::new(
            "zlibrary_guest_unavailable",
            "Z-Library refused guest access to this operation; no account login will be attempted",
            4,
        ));
    }
    if !status.is_success() {
        return Err(unavailable(&format!(
            "Z-Library metadata HTTP {}",
            status.as_u16()
        )));
    }
    let value = parsed.map_err(|_| malformed())?;
    if value["success"] != 1 && value["success"] != true {
        let error = format!("{} {}", value["error"], value["message"]).to_lowercase();
        if error.contains("limit") || error.contains("quota") {
            return Err(quota());
        }
        if error.contains("auth") || error.contains("login") || error.contains("sign in") {
            return Err(Error::new(
                "zlibrary_guest_unavailable",
                "Z-Library does not permit this operation for guests; no account login will be attempted",
                4,
            ));
        }
        return Err(malformed());
    }
    Ok(value)
}
fn metadata(request: RequestBuilder, deadline: Instant) -> Result<Value> {
    let response = request
        .timeout(remaining(deadline, Duration::from_secs(15))?)
        .send()
        .map_err(|_| unavailable("Z-Library guest metadata request"))?;
    let status = response.status();
    let mut body = Vec::new();
    response
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|_| unavailable("Z-Library metadata body"))?;
    if body.len() > 4 * 1024 * 1024 {
        return Err(malformed());
    }
    decode(status, &body)
}
fn identifiers(reference: &str) -> Result<(&str, &str)> {
    let parts = reference
        .strip_prefix("zlibrary:")
        .and_then(|s| s.split_once(':'))
        .filter(|(id, hash)| {
            !id.is_empty()
                && id.len() <= 20
                && id.bytes().all(|b| b.is_ascii_digit())
                && !hash.is_empty()
                && hash.len() <= 64
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
        });
    parts.ok_or_else(|| Error::invalid("Expected zlibrary:<numeric ID>:<hex hash>"))
}
pub(crate) fn parse_release(v: &Value) -> Result<Release> {
    let id = v["id"]
        .as_u64()
        .map(|n| n.to_string())
        .or_else(|| v["id"].as_str().map(str::to_owned))
        .ok_or_else(malformed)?;
    let hash = v["hash"].as_str().ok_or_else(malformed)?;
    let reference = format!("zlibrary:{id}:{hash}");
    identifiers(&reference).map_err(|_| malformed())?;
    let title = v["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 2000)
        .ok_or_else(malformed)?;
    let size = v["filesize"]
        .as_u64()
        .or_else(|| v["filesize"].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(malformed)?;
    let format = v["extension"]
        .as_str()
        .ok_or_else(malformed)?
        .to_uppercase();
    let mut restrictions: Vec<String> = Vec::new();
    if format != "EPUB" {
        restrictions.push("no_epub".into());
    }
    if size == 0 || size > MAX_EPUB_BYTES {
        restrictions.push("download_too_large_or_empty".into());
    }
    if v["deleted"] == true || v["deleted"] == 1 || v["active"] == false || v["active"] == 0 {
        restrictions.push("unavailable".into());
    }
    let lang = v["language"].as_str().and_then(|s| {
        language(if s.eq_ignore_ascii_case("brazilian") {
            "pt-br"
        } else {
            s
        })
        .ok()
    });
    Ok(Release {
        id: reference,
        source: "zlibrary".into(),
        title: title.into(),
        author: v["author"].as_str().unwrap_or_default().into(),
        language: lang,
        format,
        size_bytes: size,
        seeders: 0,
        eligible: restrictions.is_empty(),
        restrictions,
    })
}
pub fn search(c: &Config, query: &str, deadline: Instant) -> Result<Value> {
    let z = configured(c)?;
    let url = origin(&z.base_url)?
        .join("eapi/book/search")
        .map_err(|_| malformed())?;
    let data = metadata(
        client()?.post(url).form(&[
            ("message", query),
            ("extensions[]", "epub"),
            ("page", "1"),
            ("limit", "50"),
        ]),
        deadline,
    )?;
    let rows = data["books"]
        .as_array()
        .filter(|r| r.len() <= 50)
        .ok_or_else(malformed)?;
    let releases = rows.iter().map(parse_release).collect::<Result<Vec<_>>>()?;
    let total = data["pagination"]["total_items"]
        .as_u64()
        .unwrap_or(rows.len() as u64);
    let truncated = total > rows.len() as u64
        || data["pagination"]["next"].as_u64().is_some()
        || (data["pagination"]["total_items"].is_null() && rows.len() == 50);
    Ok(json!({"releases":releases,"reported_total":total,"truncated":truncated}))
}
pub fn check(c: &Config) -> Result<()> {
    search(
        c,
        "Alice in Wonderland",
        Instant::now() + Duration::from_secs(15),
    )
    .map(|_| ())
}

pub(crate) fn download_url(z: &Zlibrary, input: &str) -> Result<Url> {
    let base = origin(&z.base_url)?;
    let url = base
        .join(input)
        .map_err(|_| Error::new("unsafe_download_url", "Invalid Z-Library download URL", 6))?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.host_str().is_some_and(|host| {
            public_hostname(host)
                && (Some(host) == base.host_str() || z.download_hosts.iter().any(|h| h == host))
        })
    {
        return Err(Error::new(
            "unsafe_download_url",
            "Z-Library download host or redirect is not trusted; the owner must verify any additional download host",
            6,
        ));
    }
    Ok(url)
}
fn file_response(http: &Client, z: &Zlibrary, mut url: Url, deadline: Instant) -> Result<Response> {
    for _ in 0..=3 {
        let response = http
            .get(url.clone())
            .timeout(remaining(deadline, Duration::from_secs(60))?)
            .send()
            .map_err(|_| unavailable("Z-Library guest download"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .ok_or_else(malformed)?;
            let next = url.join(location).map_err(|_| malformed())?;
            url = download_url(z, next.as_str())?;
            continue;
        }
        if response.status() != reqwest::StatusCode::OK
            || response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|t| t.to_ascii_lowercase().contains("text/html"))
        {
            let status = response.status();
            let mut body = Vec::new();
            response
                .take(65536)
                .read_to_end(&mut body)
                .map_err(|_| unavailable("Z-Library download error response"))?;
            decode(status, &body)?;
            return Err(Error::new(
                "zlibrary_guest_unavailable",
                "Z-Library returned a page instead of an EPUB; no book was sent",
                4,
            ));
        }
        return Ok(response);
    }
    Err(Error::new(
        "unsafe_download_url",
        "Z-Library exceeded the limit of three download redirects",
        6,
    ))
}
pub(crate) fn validate_epub(path: &Path) -> Result<()> {
    if !(1..=MAX_EPUB_BYTES).contains(&fs::metadata(path)?.len()) {
        return Err(Error::new(
            "unsafe_epub",
            "Guest download is empty or exceeds 30 MiB",
            6,
        ));
    }
    let mut zip = zip::ZipArchive::new(fs::File::open(path)?)
        .map_err(|_| Error::new("unsafe_epub", "Guest download is not a ZIP/EPUB file", 6))?;
    let mut mime = String::new();
    zip.by_name("mimetype")
        .map_err(|_| Error::new("unsafe_epub", "Guest download lacks the EPUB mimetype", 6))?
        .take(64)
        .read_to_string(&mut mime)
        .map_err(|_| Error::new("unsafe_epub", "Invalid EPUB mimetype", 6))?;
    if mime.trim() != "application/epub+zip" {
        return Err(Error::new(
            "unsafe_epub",
            "Guest download is not an EPUB",
            6,
        ));
    }
    Ok(())
}
pub(crate) fn save_file(
    reader: impl Read,
    mut part: fs::File,
    partial: &Path,
    target: &Path,
    expected: Option<u64>,
) -> Result<()> {
    if expected.is_some_and(|n| n > MAX_EPUB_BYTES) {
        return Err(Error::new(
            "download_too_large",
            "Z-Library EPUB exceeds 30 MiB",
            6,
        ));
    }
    let n = std::io::copy(&mut reader.take(MAX_EPUB_BYTES + 1), &mut part).map_err(|_| {
        Error::new(
            "zlibrary_download_interrupted",
            "Guest download was interrupted; an owner retry may consume another guest slot",
            4,
        )
    })?;
    if n > MAX_EPUB_BYTES {
        return Err(Error::new(
            "download_too_large",
            "Z-Library EPUB exceeds 30 MiB",
            6,
        ));
    }
    if expected.is_some_and(|expected| expected != n) {
        return Err(Error::new(
            "zlibrary_download_interrupted",
            "Guest download length does not match the response; an owner retry is required",
            4,
        ));
    }
    part.flush()?;
    part.sync_all()?;
    validate_epub(partial)?;
    fs::rename(partial, target)?;
    fs::File::open(
        target
            .parent()
            .ok_or_else(|| Error::invalid("Missing download directory"))?,
    )?
    .sync_all()?;
    Ok(())
}
pub fn download(c: &Config, reference: &str, dir: &Path) -> Result<()> {
    let z = configured(c)?;
    let (id, hash) = identifiers(reference)?;
    fs::create_dir_all(dir)?;
    let target = dir.join("source.epub");
    if target.try_exists()? {
        return validate_epub(&target);
    }
    let partial = dir.join("source.epub.part");
    // Persist the attempt before remote work. A crash/uncertain download must not spend guest quota automatically.
    let part = fs::OpenOptions::new().write(true).create_new(true).open(&partial)
        .map_err(|e| if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::new("zlibrary_download_interrupted", "An earlier guest download was interrupted; an explicit owner retry is required", 4)
        } else { e.into() })?;
    part.sync_all()?;
    fs::File::open(dir)?.sync_all()?;
    let result = (|| {
        let http = client()?;
        let url = origin(&z.base_url)?
            .join(&format!("eapi/book/{id}/{hash}"))
            .map_err(|_| malformed())?;
        let data = metadata(http.get(url), Instant::now() + Duration::from_secs(15))?;
        let release = parse_release(&data["book"])?;
        if release.id != reference || !release.eligible {
            return Err(Error::new(
                "release_restricted",
                "Selected Z-Library book is unavailable, oversized or not an EPUB",
                6,
            ));
        }
        let link = data["book"]["dl"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "exactEnd")
            .ok_or_else(|| {
                Error::new(
                    "zlibrary_guest_unavailable",
                    "Z-Library did not provide a guest download link",
                    4,
                )
            })?;
        let response = file_response(
            &http,
            z,
            download_url(z, link)?,
            Instant::now() + Duration::from_secs(60),
        )?;
        let expected = response.content_length();
        save_file(response, part, &partial, &target, expected)
    })();
    result.map_err(|mut e: Error| {
        e.retryable = false;
        e
    })
}
pub fn reset_partial(c: &Config, job_id: &str) -> Result<()> {
    uuid::Uuid::parse_str(job_id).map_err(|_| Error::invalid("Invalid job ID"))?;
    match fs::remove_file(c.processing_dir.join(job_id).join("source.epub.part")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
