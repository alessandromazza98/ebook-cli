use crate::{
    config::language,
    error::{Error, Result},
};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Component, Path},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[allow(dead_code, clippy::question_mark)]
#[path = "../vendor/kindling_repair.rs"]
mod kindling;

fn unsafe_epub(message: &str) -> Error {
    Error::new("unsafe_epub", message, 6)
}
fn xml(s: &str) -> Result<roxmltree::Document<'_>> {
    if s.contains("<!ENTITY")
        || s.split("<!DOCTYPE")
            .skip(1)
            .any(|d| d.split('>').next().unwrap_or("").contains('['))
    {
        return Err(unsafe_epub("EPUB entity declarations are unsupported"));
    }
    // Ordinary HTML doctypes are common. roxmltree never fetches external DTDs.
    roxmltree::Document::parse_with_options(
        s,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|_| unsafe_epub("Malformed EPUB XML"))
}
fn decode(b: &[u8]) -> Result<String> {
    let head = String::from_utf8_lossy(&b[..b.len().min(2048)]);
    let decl = regex::Regex::new(r#"(?i)(?:encoding\s*=\s*["']|charset\s*=\s*["']?)([a-z0-9_-]+)"#)
        .unwrap();
    let cap = decl.captures(&head);
    let explicit = cap.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str());
    let (encoding, skip) = if let Some((e, n)) = encoding_rs::Encoding::for_bom(b) {
        (e, n)
    } else if let Some(label) = explicit {
        (
            encoding_rs::Encoding::for_label(label.as_bytes())
                .ok_or_else(|| unsafe_epub("Unsupported declared text encoding"))?,
            0,
        )
    } else {
        (encoding_rs::UTF_8, 0)
    };
    let (s, errors) = encoding.decode_without_bom_handling(&b[skip..]);
    if errors {
        return Err(unsafe_epub(
            "Text encoding cannot be decoded without data loss",
        ));
    }
    Ok(decl
        .replace_all(&s, |c: &regex::Captures| c[0].replace(&c[1], "utf-8"))
        .into_owned())
}
pub fn prepare(input: &Path, output: &Path, selected_language: &str) -> Result<serde_json::Value> {
    let lang = language(selected_language)?;
    if input == output || output.exists() {
        return Err(Error::invalid(
            "EPUB preparation requires a new, separate output file",
        ));
    }
    if fs::metadata(input)?.len() > 30 * 1024 * 1024 {
        return Err(unsafe_epub("EPUB exceeds 30 MiB attachment limit"));
    }
    let mut archive = ZipArchive::new(fs::File::open(input)?)
        .map_err(|_| unsafe_epub("Invalid EPUB ZIP archive"))?;
    if archive.len() > 4096 {
        return Err(unsafe_epub("Too many EPUB entries"));
    }
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut names = HashSet::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut e = archive
            .by_index(i)
            .map_err(|_| unsafe_epub("Encrypted or unreadable ZIP entry"))?;
        let name = e.name().to_string();
        let path = Path::new(&name);
        if name.contains(['\\', '\0', ':'])
            || path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
                && !e.is_dir()
            || e.enclosed_name().is_none()
        {
            return Err(unsafe_epub("Archive path traversal or invalid entry name"));
        }
        if !names.insert(name.to_lowercase())
            || e.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
        {
            return Err(unsafe_epub("Duplicate archive path or symlink"));
        }
        if matches!(
            name.to_lowercase().as_str(),
            "meta-inf/encryption.xml" | "meta-inf/rights.xml"
        ) {
            return Err(unsafe_epub("Encrypted EPUB is unsupported"));
        }
        if e.is_dir() {
            continue;
        }
        total = total
            .checked_add(e.size())
            .ok_or_else(|| unsafe_epub("Archive size overflow"))?;
        if total > 128 * 1024 * 1024
            || e.size() > 32 * 1024 * 1024
            || e.size() > e.compressed_size().max(1) * 200
        {
            return Err(unsafe_epub("EPUB decompression limit exceeded"));
        }
        let mut b = Vec::new();
        e.by_ref().take(32 * 1024 * 1024 + 1).read_to_end(&mut b)?;
        if b.len() as u64 != e.size() {
            return Err(unsafe_epub("ZIP entry size mismatch"));
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if matches!(
            ext.as_str(),
            "xml" | "opf" | "ncx" | "xhtml" | "html" | "htm" | "css" | "svg"
        ) {
            let s = decode(&b)?;
            if matches!(
                ext.as_str(),
                "xml" | "opf" | "ncx" | "xhtml" | "html" | "htm" | "svg"
            ) {
                let document = xml(&s)?;
                if document.descendants().filter(|n| n.is_element()).any(|n| {
                    n.attributes().any(|a| {
                        a.name().to_lowercase().starts_with("on")
                            || a.value().to_lowercase().contains("javascript:")
                    })
                }) {
                    return Err(unsafe_epub("Active EPUB attributes are unsupported"));
                }
            }
            if matches!(ext.as_str(), "html" | "htm" | "xhtml" | "svg") {
                let lower = s.to_lowercase();
                if lower.contains("<script")
                    || lower.contains("<iframe")
                    || lower.contains("javascript:")
                    || lower.contains("<object")
                    || lower.contains("<embed")
                {
                    return Err(unsafe_epub("Active EPUB content is unsupported"));
                }
            }
            files.insert(name, s.into_bytes());
        } else if ext == "js" {
            return Err(unsafe_epub("Executable EPUB scripts are unsupported"));
        } else {
            files.insert(name, b);
        }
    }
    if files.get("mimetype").map(Vec::as_slice) != Some(b"application/epub+zip") {
        return Err(unsafe_epub("Missing or invalid EPUB mimetype"));
    }
    let container = String::from_utf8(
        files
            .get("META-INF/container.xml")
            .ok_or_else(|| unsafe_epub("Missing container.xml"))?
            .clone(),
    )
    .map_err(|_| unsafe_epub("Invalid container encoding"))?;
    let doc = xml(&container)?;
    let roots: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("rootfile"))
        .collect();
    if roots.len() != 1 {
        return Err(unsafe_epub("Expected exactly one EPUB rootfile"));
    }
    let root = roots[0];
    let opf_path = root
        .attribute("full-path")
        .ok_or_else(|| unsafe_epub("Missing OPF path"))?
        .to_string();
    // CWA fix_container_xml: Kindle requires this rootfile media type.
    if root.attribute("media-type") != Some("application/oebps-package+xml") {
        let range = root.range();
        let mut tag = container[range.clone()].to_string();
        let re = regex::Regex::new(r#"\s+media-type\s*=\s*(?:"[^"]*"|'[^']*')"#).unwrap();
        tag = re.replace(&tag, "").into_owned();
        let pos = tag
            .find('>')
            .ok_or_else(|| unsafe_epub("Invalid rootfile element"))?;
        let pos = if tag.as_bytes()[pos - 1] == b'/' {
            pos - 1
        } else {
            pos
        };
        tag.insert_str(pos, " media-type=\"application/oebps-package+xml\"");
        let mut fixed = container.clone();
        fixed.replace_range(range, &tag);
        files.insert("META-INF/container.xml".into(), fixed.into_bytes());
    }
    let opf = String::from_utf8(
        files
            .get(&opf_path)
            .ok_or_else(|| unsafe_epub("OPF rootfile is missing"))?
            .clone(),
    )
    .map_err(|_| unsafe_epub("Invalid OPF encoding"))?;
    let opf_doc = xml(&opf)?;
    let metadata = opf_doc
        .descendants()
        .find(|n| n.has_tag_name("metadata"))
        .ok_or_else(|| unsafe_epub("OPF metadata is missing"))?;
    let languages: Vec<_> = metadata
        .children()
        .filter(|n| n.has_tag_name(("http://purl.org/dc/elements/1.1/", "language")))
        .collect();
    let mut fixed = opf.clone();
    if languages.is_empty() {
        let r = metadata.range();
        let close = opf[r.clone()]
            .rfind("</")
            .ok_or_else(|| unsafe_epub("Empty OPF metadata block"))?
            + r.start;
        fixed.insert_str(
            close,
            &format!(
                "<dc:language xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{lang}</dc:language>"
            ),
        );
    } else {
        // Selection is authoritative. Replace complete nodes to cover empty/invalid tags.
        for n in languages.iter().rev() {
            fixed.replace_range(n.range(),&format!("<dc:language xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{lang}</dc:language>"));
        }
    }
    xml(&fixed)?;
    files.insert(opf_path, fixed.into_bytes());
    let parent = output
        .parent()
        .ok_or_else(|| Error::invalid("Output needs a parent directory"))?;
    fs::create_dir_all(parent)?;
    let intermediate = parent.join(format!(".{}.normalized.epub", uuid::Uuid::new_v4()));
    let repaired = parent.join(format!(".{}.repaired.epub", uuid::Uuid::new_v4()));
    let result = (|| {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&intermediate)?;
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().last_modified_time(zip::DateTime::default());
        writer
            .start_file(
                "mimetype",
                options.compression_method(CompressionMethod::Stored),
            )
            .map_err(|_| unsafe_epub("Cannot write EPUB"))?;
        writer.write_all(b"application/epub+zip")?;
        for (name, b) in &files {
            if name == "mimetype" {
                continue;
            }
            writer
                .start_file(
                    name,
                    options.compression_method(CompressionMethod::Deflated),
                )
                .map_err(|_| unsafe_epub("Cannot write EPUB entry"))?;
            writer.write_all(b)?;
        }
        writer
            .finish()
            .map_err(|_| unsafe_epub("Cannot finish EPUB"))?
            .sync_all()?;
        let report = kindling::repair_epub(&intermediate, &repaired)
            .map_err(|_| unsafe_epub("Kindling repair rejected EPUB structure"))?;
        fs::File::open(&repaired)?.sync_all()?;
        if fs::metadata(&repaired)?.len() > 30 * 1024 * 1024 {
            return Err(unsafe_epub("Prepared EPUB exceeds attachment limit"));
        }
        // Atomic no-clobber publication also protects aliases of the source.
        fs::hard_link(&repaired, output)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(
            serde_json::json!({"kindling":serde_json::from_str::<serde_json::Value>(&report.to_json())?,"language":lang}),
        )
    })();
    for temp in [&intermediate, &repaired] {
        if let Err(e) = fs::remove_file(temp)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("EPUB temporary-file cleanup failed ({:?})", e.kind());
            if result.is_ok() {
                return Err(e.into());
            }
        }
    }
    result
}
