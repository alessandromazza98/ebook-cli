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
    cookie::Jar,
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_EPUB_BYTES: u64 = 30 * 1024 * 1024;

struct PublicDns;
impl reqwest::dns::Resolve for PublicDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((name.as_str(), 0)),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "Z-Library DNS timed out")
            })??;
            let addresses = match public_addresses(addresses.take(33).collect()) {
                Ok(addresses) => addresses,
                Err(error) => {
                    eprintln!(
                        "{}",
                        json!({"event":"provider_dns_rejected","source":"zlibrary","host":name.as_str()})
                    );
                    return Err(error.into());
                }
            };
            // The connector uses these checked addresses directly, retaining the original TLS hostname.
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}
pub(crate) fn public_addresses(addresses: Vec<SocketAddr>) -> std::io::Result<Vec<SocketAddr>> {
    let global = |ip: IpAddr| match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && a != 0
                && a < 240
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0 && c == 0)
                && !(a == 192 && b == 88 && c == 99)
                && !(a == 198 && (b == 18 || b == 19))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Public unicast only; exclude transition, protocol and documentation networks.
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    };
    if addresses.is_empty() || addresses.len() > 32 || addresses.iter().any(|a| !global(a.ip())) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Z-Library DNS must resolve only to public unicast addresses",
        ));
    }
    Ok(addresses)
}
pub(crate) fn approved_cdn(host: &str) -> bool {
    host == "ncdn.ec" || host.ends_with(".ncdn.ec")
}

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
fn client() -> Result<(Client, Arc<Jar>)> {
    let cookies = Arc::new(Jar::default());
    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(PublicDns))
        .cookie_provider(cookies.clone())
        .user_agent(concat!("ebook-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| unavailable("Z-Library HTTP client"))?;
    Ok((http, cookies))
}
fn guest_blocked() -> Error {
    Error::new(
        "zlibrary_guest_blocked",
        "Z-Library's guest check could not be completed; no file was sent",
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
    let (http, _) = client()?;
    let data = metadata(
        http.post(url).form(&[
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
        || !url.host_str().is_some_and(public_hostname)
    {
        return Err(Error::new(
            "unsafe_download_url",
            "Z-Library download URL must use HTTPS and a public DNS hostname without credentials, nondefault ports or fragments",
            6,
        ));
    }
    if !url.host_str().is_some_and(|host| {
        Some(host) == base.host_str()
            || approved_cdn(host)
            || z.download_hosts.iter().any(|h| h == host)
    }) {
        return Err(Error::new(
            "unsafe_download_url",
            format!(
                "Z-Library download host {} is not trusted; the owner must verify any additional download host",
                url.host_str().unwrap_or("<missing>")
            ),
            6,
        ));
    }
    Ok(url)
}
// This is the site's guest SHA-1 proof-of-work protocol, not a JavaScript evaluator.
// Reject unfamiliar scripts rather than executing remote code or guessing cookie values.
pub(crate) fn guest_challenge(body: &[u8]) -> Option<String> {
    if body.len() > 65536 {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;
    for marker in [
        "Checking your browser",
        "let n1=parseInt('0x'+c[0x0])",
        "s[n1]===0xb0",
        "s[n1+0x1]===0xb",
        "c_time=",
        "window.location.reload()",
        "JS_SHA1_NO_NODE_JS",
    ] {
        if !body.contains(marker) {
            return None;
        }
    }
    let pattern =
        regex::Regex::new(r"const\s+\w+=\['([A-Za-z0-9_-]{32,512})','c_token=','array'\]")
            .expect("static guest challenge pattern");
    let mut captures = pattern.captures_iter(body);
    let challenge = captures.next()?.get(1)?.as_str();
    if captures.next().is_some() || !challenge.as_bytes()[0].is_ascii_hexdigit() {
        return None;
    }
    Some(challenge.to_owned())
}
pub(crate) fn guest_proof(challenge: &str, deadline: Instant) -> Result<(String, Duration)> {
    if !(32..=512).contains(&challenge.len())
        || !challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(guest_blocked());
    }
    let index = challenge
        .chars()
        .next()
        .and_then(|c| c.to_digit(16))
        .ok_or_else(guest_blocked)? as usize;
    let start = Instant::now();
    let deadline = deadline.min(start + Duration::from_secs(5));
    let mut prefix = Sha1::new();
    prefix.update(challenge.as_bytes());
    for nonce in 0..1_000_000u32 {
        if nonce % 1024 == 0 && Instant::now() >= deadline {
            return Err(guest_blocked());
        }
        let suffix = nonce.to_string();
        let mut hash = prefix.clone();
        hash.update(suffix.as_bytes());
        let digest = hash.finalize();
        if digest[index] == 0xb0 && digest[index + 1] == 0x0b {
            return Ok((format!("{challenge}{suffix}"), start.elapsed()));
        }
    }
    Err(guest_blocked())
}
fn file_response(
    http: &Client,
    cookies: &Jar,
    z: &Zlibrary,
    mut url: Url,
    deadline: Instant,
) -> Result<Response> {
    let mut redirects = 0;
    let mut verified = false;
    loop {
        let response = http
            .get(url.clone())
            .timeout(remaining(deadline, Duration::from_secs(60))?)
            .send()
            .map_err(|_| unavailable("Z-Library guest download"))?;
        if response.status().is_redirection() {
            if redirects == 3 {
                return Err(Error::new(
                    "unsafe_download_url",
                    "Z-Library exceeded the limit of three download redirects",
                    6,
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .ok_or_else(malformed)?;
            let next = url.join(location).map_err(|_| malformed())?;
            url = download_url(z, next.as_str())?;
            redirects += 1;
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
                .take(65537)
                .read_to_end(&mut body)
                .map_err(|_| unavailable("Z-Library download error response"))?;
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                && !verified
                && let Some(challenge) = guest_challenge(&body)
            {
                let (proof, elapsed) = guest_proof(&challenge, deadline)?;
                // Host-only, secure, operation-local guest cookies. Never persist or log them.
                cookies.add_cookie_str(&format!("c_token={proof}; Path=/; Secure"), &url);
                cookies.add_cookie_str(
                    &format!("c_time={}; Path=/; Secure", elapsed.as_secs_f64()),
                    &url,
                );
                verified = true;
                continue;
            }
            decode(status, &body)?;
            return Err(Error::new(
                "zlibrary_guest_unavailable",
                "Z-Library returned a page instead of an EPUB; no book was sent",
                4,
            ));
        }
        return Ok(response);
    }
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
        let (http, cookies) = client()?;
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
            &cookies,
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
