use crate::{
    config::language,
    error::{Error, Result},
    integrations::Release,
};
use serde_json::{Value, json};
fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn title_language(
    title: &str,
    candidates: &std::collections::HashSet<Option<&String>>,
) -> Option<String> {
    let title = norm(title);
    let tokens: Vec<_> = title.split_whitespace().collect();
    let lexicons = [
        ("en", &["the", "of", "and", "with"][..]),
        ("fr", &["le", "la", "les", "des", "du", "et"][..]),
        ("de", &["der", "die", "das", "und", "von"][..]),
        ("es", &["el", "la", "los", "las", "del", "y"][..]),
        ("it", &["il", "lo", "gli", "della", "e"][..]),
        ("pt", &["os", "as", "dos", "das", "do", "da"][..]),
        ("ro", &["și", "din", "lui", "ale"][..]),
    ];
    let matches: Vec<_> = lexicons
        .iter()
        .filter(|(l, words)| {
            candidates
                .iter()
                .any(|c| c.is_some_and(|c| c.as_str() == *l))
                && tokens.iter().any(|t| words.contains(t))
        })
        .collect();
    if matches.len() == 1 {
        Some(matches[0].0.into())
    } else {
        None
    }
}
pub fn choose(
    releases: &[Release],
    title: &str,
    author: Option<&str>,
    lang: Option<&str>,
) -> Result<Value> {
    if title.trim().is_empty() {
        return Err(Error::invalid("Title is required"));
    }
    let explicit = lang.map(language).transpose()?;
    let mut ranked: Vec<_> = releases
        .iter()
        .filter(|r| r.eligible)
        .map(|r| {
            let exact = norm(&r.title) == norm(title);
            let author_match = author.is_none_or(|a| norm(a) == norm(&r.author));
            let lang_match = explicit
                .as_ref()
                .is_none_or(|l| r.language.as_ref() == Some(l));
            let a = norm(&r.title);
            let b = norm(title);
            let a: std::collections::HashSet<_> = a.split_whitespace().collect();
            let b: std::collections::HashSet<_> = b.split_whitespace().collect();
            let similarity = (100 * a.intersection(&b).count() / a.union(&b).count().max(1)) as i32;
            let score = similarity
                + if author.is_some() && author_match {
                    40
                } else {
                    0
                }
                + if lang_match { 20 } else { 0 };
            (r, exact && author_match && lang_match, score)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(b.0.seeders.cmp(&a.0.seeders))
            .then(a.0.size_bytes.cmp(&b.0.size_bytes))
            .then(a.0.id.cmp(&b.0.id))
    });
    let matches: Vec<_> = ranked.iter().filter(|x| x.1).map(|x| x.0).collect();
    let languages: std::collections::HashSet<_> =
        matches.iter().map(|r| r.language.as_ref()).collect();
    let authors: std::collections::HashSet<_> = matches.iter().map(|r| norm(&r.author)).collect();
    let title_inference = title_language(title, &languages);
    let inferred = explicit.or_else(|| title_inference.clone()).or_else(|| {
        if languages.len() == 1 {
            matches.first().and_then(|r| r.language.clone())
        } else {
            None
        }
    });
    if !matches.is_empty()
        && inferred.is_some()
        && authors.len() == 1
        && !authors.contains("")
        && let Some(best) = matches.iter().find(|r| r.language == inferred)
    {
        return Ok(
            json!({"status":"selected","release_id":best.id,"language":inferred,"language_source":if lang.is_some(){"explicit"}else if title_inference.is_some(){"title_and_matching_metadata"}else{"matching_metadata"}}),
        );
    }
    Ok(
        json!({"status":"clarification_required","reason":if matches.is_empty(){"no_clear_title_author_language_match"}else if authors.len()>1{"ambiguous_author"}else{"language_or_author_required"},"candidates":ranked.into_iter().take(20).map(|(r,_,score)|json!({"release":r,"score":score})).collect::<Vec<_>>()}),
    )
}
