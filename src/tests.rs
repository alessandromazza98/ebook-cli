use crate::{
    config::Config,
    db::Db,
    epub,
    integrations::{Release, parse_release},
    selection, torrent, worker,
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use zip::{ZipWriter, write::SimpleFileOptions};
fn config(root: &Path) -> Config {
    let mut c = Config::example();
    c.state_dir = root.join("state");
    c.processing_dir = root.join("processing");
    c.qbittorrent.local_dir = root.join("ebook-cli");
    c
}
fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cwa")
}
fn make_epub(path: &Path, extras: Vec<(&str, Vec<u8>)>) {
    let mut z = ZipWriter::new(fs::File::create(path).unwrap());
    let entries=vec![("mimetype",b"application/epub+zip".to_vec()),("META-INF/container.xml",br#"<container><rootfiles><rootfile full-path="book.opf"/></rootfiles></container>"#.to_vec()),("book.opf",br#"<package><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Test</dc:title></metadata></package>"#.to_vec()),("book.xhtml",br#"<html><body id="top"><img/><p>book</p></body></html>"#.to_vec())];
    for (name, data) in entries.into_iter().chain(extras) {
        z.start_file(name, SimpleFileOptions::default()).unwrap();
        z.write_all(&data).unwrap();
    }
    z.finish().unwrap();
}
#[test]
fn cwa_valid_fixtures_and_source_preserved() {
    for name in [
        "test_minimal_valid.epub",
        "test_rich_metadata.epub",
        "alice_in_wonderland.epub",
    ] {
        let t = tempfile::tempdir().unwrap();
        let source = fixture().join(name);
        let before = fs::read(&source).unwrap();
        epub::prepare(&source, &t.path().join("output.epub"), "fr").unwrap();
        assert_eq!(before, fs::read(source).unwrap());
    }
}
#[test]
fn cwa_invalid_fixtures_rejected() {
    for name in [
        "test_no_mimetype.epub",
        "test_corrupted.epub",
        "test_empty.epub",
    ] {
        let t = tempfile::tempdir().unwrap();
        assert!(
            epub::prepare(&fixture().join(name), &t.path().join("output.epub"), "en").is_err(),
            "{name}"
        );
    }
}
#[test]
fn missing_language_uses_selection_and_repair_is_idempotent() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src.epub");
    let out = t.path().join("out.epub");
    let again = t.path().join("again.epub");
    make_epub(&src, vec![]);
    epub::prepare(&src, &out, "de").unwrap();
    epub::prepare(&out, &again, "de").unwrap();
    let mut zip = zip::ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
    let mut opf = String::new();
    zip.by_name("book.opf")
        .unwrap()
        .read_to_string(&mut opf)
        .unwrap();
    assert!(opf.contains(">de</dc:language>"));
    assert!(!opf.contains(">en</dc:language>"));
    assert_eq!(fs::read(&out).unwrap(), fs::read(&again).unwrap());
}
#[test]
fn traversal_drm_entities_and_bombs_fail() {
    for (name, bytes) in [
        ("../escape", vec![1]),
        ("META-INF/encryption.xml", b"<encryption/>".to_vec()),
        (
            "entity.xhtml",
            br#"<!DOCTYPE html [<!ENTITY boom "payload">]><html>&boom;</html>"#.to_vec(),
        ),
        ("bomb.txt", vec![0; 33 * 1024 * 1024]),
    ] {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src.epub");
        make_epub(&src, vec![(name, bytes)]);
        assert!(
            epub::prepare(&src, &t.path().join("out.epub"), "en").is_err(),
            "{name}"
        );
        assert!(!t.path().join("escape").exists());
    }
}
#[test]
fn explicit_encoding_is_preserved_as_utf8() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src.epub");
    let out = t.path().join("out.epub");
    let mut text = b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><html><body>caf".to_vec();
    text.extend_from_slice(b"\xe9</body></html>");
    make_epub(&src, vec![("latin.xhtml", text)]);
    epub::prepare(&src, &out, "fr").unwrap();
    let mut z = zip::ZipArchive::new(fs::File::open(out).unwrap()).unwrap();
    let mut s = String::new();
    z.by_name("latin.xhtml")
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    assert!(s.contains("café"));
    assert!(s.contains("utf-8"));
}
#[test]
fn database_restart_and_send_uncertainty() {
    let t = tempfile::tempdir().unwrap();
    let c = config(t.path());
    let mut db = Db::open(&c).unwrap();
    let j = db.submit("mam:1", "en", "key").unwrap();
    assert_eq!(j.id, db.submit("mam:1", "en", "other-key").unwrap().id);
    assert!(db.submit("mam:2", "en", "key").is_err());
    db.state(&j.id, "sending", None).unwrap();
    drop(db);
    let db = Db::open(&c).unwrap();
    db.recover().unwrap();
    assert_eq!(db.job(&j.id).unwrap().state, "send_uncertain");
    assert!(db.retry(&j.id, false).is_err());
    assert_eq!(db.retry(&j.id, true).unwrap().state, "ready");
    db.state(&j.id, "smtp_accepted", None).unwrap();
    assert!(db.retry(&j.id, true).is_err());
    assert_eq!(db.delivered(&j.id).unwrap().state, "delivered");
}
#[test]
fn worker_lock_prevents_parallel_sends() {
    let t = tempfile::tempdir().unwrap();
    let c = config(t.path());
    let lock = worker::lock(&c).unwrap();
    assert!(worker::lock(&c).is_err());
    drop(lock);
    assert!(worker::lock(&c).is_ok());
}
fn release(id: &str, author: &str, lang: &str) -> Release {
    Release {
        id: id.into(),
        source: "mam".into(),
        title: "Heretics of Dune".into(),
        author: author.into(),
        language: Some(lang.into()),
        format: "EPUB".into(),
        size_bytes: 500000,
        seeders: 10,
        restrictions: vec![],
        eligible: true,
    }
}
#[test]
fn selection_language_author_and_ambiguity() {
    let r = vec![
        release("mam:1", "Frank Herbert", "en"),
        release("mam:2", "Frank Herbert", "fr"),
    ];
    assert_eq!(
        selection::choose(&r, "Heretics of Dune", Some("Frank Herbert"), Some("fr")).unwrap()["release_id"],
        "mam:2"
    );
    assert_eq!(
        selection::choose(&r, "Heretics of Dune", None, None).unwrap()["language"],
        "en"
    );
    let mut ambiguous = r.clone();
    for r in &mut ambiguous {
        r.title = "Dune".into();
    }
    assert_eq!(
        selection::choose(&ambiguous, "Dune", None, None).unwrap()["status"],
        "clarification_required"
    );
    assert_eq!(
        selection::choose(&r[..1], "Heretics of Dune", Some("Frank Herbert"), None).unwrap()["language"],
        "en"
    );
    assert_eq!(
        selection::choose(&r, "Heretics of Dune", Some("Wrong Author"), Some("en")).unwrap()["status"],
        "not_found"
    );
}
#[test]
fn mam_metadata_restrictions_and_malformed_data() {
    let v = json!({"id":42,"title":"Title","author_info":"{\"12\":\"Author\"}","lang_code":"eng","filetype":"EPUB","seeders":"8","size":"1.2 MiB","vip":"1"});
    let r = parse_release(&v, false).unwrap();
    assert_eq!(r.language.as_deref(), Some("en"));
    assert_eq!(r.author, "Author");
    assert!(!r.eligible);
    assert!(parse_release(&v, true).unwrap().eligible);
    let mut bad = v;
    bad["seeders"] = json!("invalid");
    assert!(parse_release(&bad, true).is_err());
}
#[test]
fn metainfo_hash_validation() {
    assert!(torrent::info_hash(b"d4:infod6:pieces0:ee").is_ok());
    for b in [
        b"<html>expired session</html>".as_slice(),
        b"d4:infod6:pieces0:eejunk",
        b"d4:infod6:pieces0:e4:infodee",
        b"d4:infod1:ai01eee",
    ] {
        assert!(torrent::info_hash(b).is_err());
    }
}

#[test]
fn config_enforces_isolation_and_tls() {
    let t = tempfile::tempdir().unwrap();
    let mut c = config(t.path());
    c.mam.session = "test-session".into();
    c.smtp.host = "smtp.example.com".into();
    c.smtp.from = "sender@example.com".into();
    c.smtp.kindle = "reader@example.com".into();
    assert!(c.validate().is_ok());
    c.qbittorrent.category = "chaptarr".into();
    assert!(c.validate().is_err());
    c.qbittorrent.category = "ebook-cli".into();
    c.smtp.tls = "none".into();
    assert!(c.validate().is_err());
    c.smtp.tls = "starttls".into();
    c.processing_dir = c.qbittorrent.local_dir.clone();
    assert!(c.validate().is_err());
}

#[test]
fn preparation_never_overwrites_source_or_existing_output() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source.epub");
    make_epub(&source, vec![]);
    let before = fs::read(&source).unwrap();
    assert!(epub::prepare(&source, &t.path().join("./source.epub"), "en").is_err());
    assert_eq!(before, fs::read(&source).unwrap());
}

#[test]
fn qbittorrent_add_acknowledgements_validate_counts_and_identity() {
    use crate::integrations::add_acknowledged;
    use reqwest::StatusCode;
    assert!(add_acknowledged(StatusCode::OK, b"Ok.", "abc"));
    assert!(add_acknowledged(StatusCode::NO_CONTENT, b"", "abc"));
    let body = serde_json::to_vec(
        &json!({"success_count":1,"failure_count":0,"pending_count":0,"added_torrent_ids":["abc"]}),
    )
    .unwrap();
    assert!(add_acknowledged(StatusCode::OK, &body, "abc"));
    assert!(!add_acknowledged(StatusCode::OK, &body, "different"));
    assert!(!add_acknowledged(StatusCode::CONFLICT, &body, "abc"));
    assert!(!add_acknowledged(StatusCode::OK, b"Fails.", "abc"));
    assert!(!add_acknowledged(
        StatusCode::OK,
        br#"{"success_count":1,"failure_count":1,"pending_count":0,"added_torrent_ids":["abc"]}"#,
        "abc"
    ));
}

#[test]
fn api_quota_is_persistent_atomic_and_does_not_charge_replays() {
    let t = tempfile::tempdir().unwrap();
    let c = config(t.path());
    let mut db = Db::open(&c).unwrap();
    let job = db.submit_limited("mam:1", "en", "one", Some(1)).unwrap();
    assert_eq!(
        db.submit_limited("mam:1", "en", "one", Some(1)).unwrap().id,
        job.id
    );
    assert_eq!(
        db.submit_limited("mam:1", "en", "other", Some(1))
            .unwrap()
            .id,
        job.id
    );
    drop(db);
    let mut db = Db::open(&c).unwrap();
    assert_eq!(
        db.submit_limited("mam:2", "en", "two", Some(1))
            .unwrap_err()
            .code,
        "daily_quota_exceeded"
    );
    assert_eq!(db.jobs().unwrap().len(), 1);
    db.0.execute(
        "UPDATE api_submissions SET created_at=unixepoch()-86401",
        [],
    )
    .unwrap();
    db.submit_limited("mam:2", "en", "two", Some(1)).unwrap();
    // Independent connections serialize quota checks and insertion in one transaction.
    db.0.execute("DELETE FROM api_submissions", []).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (3..5)
        .map(|n| {
            let c = c.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut db = Db::open(&c).unwrap();
                barrier.wait();
                db.submit_limited(&format!("mam:{n}"), "en", &format!("key-{n}"), Some(1))
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count(),
        1
    );
}

#[test]
fn torrent_download_size_checks_single_and_multi_file_lengths() {
    let single = b"d4:infod6:lengthi5e4:name1:x6:pieces0:ee";
    assert!(torrent::validate_download(single, 5).is_ok());
    assert_eq!(
        torrent::validate_download(single, 4).unwrap_err().code,
        "download_too_large"
    );
    let multi = b"d4:infod5:filesld6:lengthi3e4:pathl1:aeed6:lengthi4e4:pathl1:beee6:pieces0:ee";
    assert!(torrent::validate_download(multi, 7).is_ok());
    assert_eq!(
        torrent::validate_download(multi, 6).unwrap_err().code,
        "download_too_large"
    );
    for malformed in [
        b"d4:infod6:pieces0:ee".as_slice(),
        b"d4:infod6:lengthi-1e6:pieces0:ee",
        b"d4:infod6:lengthi0e6:pieces0:ee",
        b"d4:infod5:filesle6:lengthi1e6:pieces0:ee",
        b"d4:infod5:filesld4:pathl1:aeee6:pieces0:ee",
        b"d4:infod5:filesld6:lengthi9223372036854775807eed6:lengthi9223372036854775807eed6:lengthi9223372036854775807eee6:pieces0:ee",
    ] { assert!(torrent::validate_download(malformed, u64::MAX).is_err()); }
}

#[test]
fn legacy_config_defaults_to_safe_api_limits() {
    let mut value = serde_json::to_value(Config::example()).unwrap();
    value.as_object_mut().unwrap().remove("api");
    value.as_object_mut().unwrap().remove("max_torrent_bytes");
    let c: Config = serde_json::from_value(value).unwrap();
    assert!(c.api.client_token.is_none());
    assert_eq!(c.api.requests_per_minute, 30);
    assert_eq!(c.api.jobs_per_day, 10);
    assert_eq!(c.max_torrent_bytes, 100 * 1024 * 1024);
}

fn search_result(releases: &[Release]) -> serde_json::Value {
    json!({"releases":releases,"reported_total":releases.len(),"truncated":false})
}
fn guest_config() -> crate::config::Zlibrary {
    crate::config::Zlibrary {
        enabled: true,
        base_url: "https://z-library.ec".into(),
        download_hosts: vec![],
    }
}
#[test]
fn source_policy_respects_explicit_selection_and_relevant_mam_results() {
    use crate::core::{Source, search_sources};
    let mam = release("mam:1", "Frank Herbert", "en");
    let result = search_sources(
        Source::Auto,
        "Heretics of Dune",
        Some("Frank Herbert"),
        Some("en"),
        || Ok(search_result(std::slice::from_ref(&mam))),
        || panic!("Relevant MAM result must not trigger fallback"),
    )
    .unwrap();
    assert_eq!(result["sources_searched"], json!(["mam"]));
    let result = search_sources(
        Source::Zlibrary,
        "Heretics of Dune",
        None,
        None,
        || panic!("Explicit Z-Library must skip MAM"),
        || Ok(search_result(&[])),
    )
    .unwrap();
    assert_eq!(result["sources_searched"], json!(["zlibrary"]));
    search_sources(
        Source::Mam,
        "Missing book",
        None,
        None,
        || Ok(search_result(&[])),
        || panic!("Explicit MAM must not fall back"),
    )
    .unwrap();
}
#[test]
fn automatic_fallback_handles_empty_unrelated_restricted_author_and_language_results() {
    use crate::core::{Source, search_sources};
    let mam = release("mam:1", "Frank Herbert", "en");
    let mut restricted = mam.clone();
    restricted.eligible = false;
    for (rows, title, author, lang) in [
        (vec![], "Dune", None, None),
        (vec![mam.clone()], "The Hobbit", None, None),
        (
            vec![mam.clone()],
            "Heretics of Dune",
            Some("Different Author"),
            None,
        ),
        (vec![mam.clone()], "Heretics of Dune", None, Some("fr")),
        (vec![restricted], "Heretics of Dune", None, None),
    ] {
        let result = search_sources(
            Source::Auto,
            title,
            author,
            lang,
            || Ok(search_result(&rows)),
            || Ok(search_result(&[])),
        )
        .unwrap();
        assert_eq!(result["sources_searched"], json!(["mam", "zlibrary"]));
        assert_eq!(result["fallback_reason"], "no_suitable_mam_match");
    }
}

#[test]
fn no_suitable_book_is_reported_as_not_found_instead_of_sending_related_books() {
    let r = release("mam:1", "Frank Herbert", "en");
    assert_eq!(
        selection::choose(&[r], "The Hobbit", Some("J. R. R. Tolkien"), Some("en")).unwrap()["status"],
        "not_found"
    );
    assert_eq!(
        selection::choose(&[], "The Hobbit", None, None).unwrap()["status"],
        "not_found"
    );
    let mut styled = release("mam:2", "J. R. R. Tolkien", "en");
    styled.title = "The Hobbit".into();
    assert!(selection::relevant(
        &styled,
        "The Hobbit",
        Some("JRR Tolkien"),
        Some("en")
    ));
    assert_eq!(
        selection::choose(&[styled], "The Hobbit", Some("JRR Tolkien"), Some("en")).unwrap()["status"],
        "clarification_required"
    );
}
#[test]
fn ambiguous_mam_results_require_clarification_and_upstream_failures_are_visible() {
    use crate::core::{Source, search_sources};
    use crate::error::Error;
    let rows = vec![
        release("mam:1", "Author One", "en"),
        release("mam:2", "Author Two", "en"),
    ];
    let result = search_sources(
        Source::Auto,
        "Heretics of Dune",
        None,
        None,
        || Ok(search_result(&rows)),
        || panic!("Plausible ambiguous results stay on MAM"),
    )
    .unwrap();
    let returned: Vec<Release> = serde_json::from_value(result["releases"].clone()).unwrap();
    assert_eq!(
        selection::choose(&returned, "Heretics of Dune", None, None).unwrap()["reason"],
        "ambiguous_author"
    );
    assert!(
        search_sources(
            Source::Auto,
            "Missing",
            None,
            None,
            || Err(Error::dependency("MAM search")),
            || panic!("MAM errors are not zero results")
        )
        .is_err()
    );
    let result = search_sources(
        Source::Auto,
        "Missing",
        None,
        None,
        || Ok(search_result(&[])),
        || Err(Error::dependency("Z-Library search")),
    )
    .unwrap();
    assert_eq!(result["warnings"][0]["source"], "zlibrary");
    assert_eq!(
        result["warnings"][0]["error"]["code"],
        "dependency_unavailable"
    );
    assert!(
        search_sources(
            Source::Zlibrary,
            "Missing",
            None,
            None,
            || panic!(),
            || Err(Error::dependency("Z-Library search"))
        )
        .is_err()
    );
}
#[test]
fn api_source_defaults_and_guest_configuration_reject_credentials() {
    use crate::core::{Action, Source};
    let action: Action = serde_json::from_value(json!({"action":"search","query":"Dune"})).unwrap();
    assert!(matches!(
        action,
        Action::Search {
            source: Source::Auto,
            ..
        }
    ));
    assert!(
        serde_json::from_value::<Action>(
            json!({"action":"search","query":"Dune","source":"unknown"})
        )
        .is_err()
    );
    let mut z = serde_json::to_value(guest_config()).unwrap();
    z["user_key"] = json!("not-accepted");
    assert!(serde_json::from_value::<crate::config::Zlibrary>(z).is_err());
    let mut old = serde_json::to_value(release("mam:1", "Author", "en")).unwrap();
    old.as_object_mut().unwrap().remove("source");
    assert_eq!(
        serde_json::from_value::<Release>(old).unwrap().source(),
        "mam"
    );
}
#[test]
fn zlibrary_metadata_is_guest_eligible_without_seeders_and_validates_identifiers() {
    let mut v = json!({"id":42,"hash":"aabbcc","title":"Title","author":"Author","language":"English","extension":"epub","filesize":1024});
    let r = crate::zlibrary::parse_release(&v).unwrap();
    assert!(r.eligible);
    assert_eq!(r.seeders, 0);
    assert_eq!(r.source, "zlibrary");
    assert_eq!(r.id, "zlibrary:42:aabbcc");
    assert_eq!(r.language.as_deref(), Some("en"));
    v["extension"] = json!("pdf");
    assert!(!crate::zlibrary::parse_release(&v).unwrap().eligible);
    v["hash"] = json!("../secret");
    assert!(crate::zlibrary::parse_release(&v).is_err());
    v["hash"] = json!("aabbcc");
    v["filesize"] = json!(-1);
    assert!(crate::zlibrary::parse_release(&v).is_err());
}
#[test]
fn zlibrary_response_errors_are_explicit_and_do_not_include_raw_payloads() {
    use crate::zlibrary::decode;
    use reqwest::StatusCode;
    assert_eq!(
        decode(
            StatusCode::SERVICE_UNAVAILABLE,
            b"<html><title>Checking your browser ...</title></html>"
        )
        .unwrap_err()
        .code,
        "zlibrary_guest_blocked"
    );
    assert_eq!(
        decode(StatusCode::TOO_MANY_REQUESTS, b"private payload")
            .unwrap_err()
            .code,
        "zlibrary_quota_exceeded"
    );
    assert_eq!(
        decode(StatusCode::OK, b"<html>Daily limit reached</html>")
            .unwrap_err()
            .code,
        "zlibrary_quota_exceeded"
    );
    let error = decode(
        StatusCode::OK,
        br#"{"success":0,"error":"Authorization failed: secret"}"#,
    )
    .unwrap_err();
    assert_eq!(error.code, "zlibrary_guest_unavailable");
    assert!(!error.message.contains("secret"));
    assert_eq!(
        decode(StatusCode::OK, b"non JSON").unwrap_err().code,
        "invalid_upstream_response"
    );
    assert!(decode(StatusCode::OK, br#"{"success":1,"books":[]}"#).is_ok());
    assert!(
        decode(
            StatusCode::OK,
            br#"{"success":1,"books":[{"title":"Checking your browser"}]}"#
        )
        .is_ok()
    );
}
#[test]
fn zlibrary_download_hosts_and_redirects_cannot_escape_owner_trust() {
    use crate::zlibrary::{download_url, origin};
    let mut z = guest_config();
    assert!(download_url(&z, "/dl/opaque").is_ok());
    for input in [
        "http://z-library.ec/dl/x",
        "https://127.0.0.1/x",
        "https://[::1]/x",
        "https://evil.example/x",
        "//evil.example/x",
        "https://user:password@z-library.ec/x",
        "https://z-library.ec:8443/x",
        "https://z-library.ec/x#fragment",
    ] {
        assert!(download_url(&z, input).is_err(), "{input}");
    }
    for input in [
        "https://127.0.0.1",
        "https://localhost",
        "https://host.local",
        "https://example.org/path",
        "https://example.org?x=1",
    ] {
        assert!(origin(input).is_err(), "{input}");
    }
    z.download_hosts.push("files.example.org".into());
    assert!(download_url(&z, "https://files.example.org/file.epub?signature=opaque").is_ok());
    assert!(download_url(&z, "https://other.files.example.org/file.epub").is_err());
    assert!(z.validate().is_ok());
}
#[test]
fn guest_download_commits_only_complete_valid_epubs() {
    let t = tempfile::tempdir().unwrap();
    let source = fs::read(fixture().join("test_minimal_valid.epub")).unwrap();
    let target = t.path().join("source.epub");
    let partial = t.path().join("source.epub.part");
    for (bytes, expected) in [
        (b"<html>Quota exhausted</html>".to_vec(), None),
        (vec![], None),
        (source.clone(), Some(source.len() as u64 + 1)),
    ] {
        let file = fs::File::create(&partial).unwrap();
        assert!(
            crate::zlibrary::save_file(bytes.as_slice(), file, &partial, &target, expected)
                .is_err()
        );
        assert!(!target.exists());
    }
    let file = fs::File::create(&partial).unwrap();
    crate::zlibrary::save_file(
        source.as_slice(),
        file,
        &partial,
        &target,
        Some(source.len() as u64),
    )
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), source);
    assert!(!partial.exists());
    let other = t.path().join("too-large.epub");
    let file = fs::File::create(&partial).unwrap();
    let huge = std::io::repeat(b'x').take(crate::zlibrary::MAX_EPUB_BYTES + 1);
    assert_eq!(
        crate::zlibrary::save_file(huge, file, &partial, &other, None)
            .unwrap_err()
            .code,
        "download_too_large"
    );
    assert!(!other.exists());
    assert_eq!(fs::read(&target).unwrap(), source);
}
#[test]
fn guest_worker_reuses_completed_download_and_rejects_interrupted_attempts_without_network() {
    let t = tempfile::tempdir().unwrap();
    let mut c = config(t.path());
    c.zlibrary = Some(guest_config());
    let mut db = Db::open(&c).unwrap();
    let j = db.submit("zlibrary:42:aabbcc", "en", "guest").unwrap();
    let dir = c.processing_dir.join(&j.id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("source.epub.part"), b"partial").unwrap();
    worker::tick(&c, &db).unwrap();
    assert_eq!(db.job(&j.id).unwrap().state, "failed");
    assert_eq!(
        db.job(&j.id).unwrap().error.unwrap()["code"],
        "zlibrary_download_interrupted"
    );
    crate::core::execute(
        &c,
        crate::core::Action::Retry {
            job_id: j.id.clone(),
            acknowledge_possible_duplicate: false,
        },
    )
    .unwrap();
    assert!(!dir.join("source.epub.part").exists());
    fs::copy(
        fixture().join("test_minimal_valid.epub"),
        dir.join("source.epub"),
    )
    .unwrap();
    worker::tick(&c, &db).unwrap();
    assert_eq!(db.job(&j.id).unwrap().state, "preparing");
    worker::tick(&c, &db).unwrap();
    assert_eq!(db.job(&j.id).unwrap().state, "ready");
    assert!(dir.join("prepared.epub").exists());
}
