use crate::error::{Error, Result};
use sha1::{Digest, Sha1};

// Hash the original info bytes, never a re-encoded dictionary. Reject noncanonical
// bencoding and bound nesting before any torrent reaches qBittorrent.
pub fn info_hash(data: &[u8]) -> Result<String> {
    fn bad() -> Error {
        Error::invalid("Malformed or unsupported v1 torrent metainfo")
    }
    fn string<'a>(d: &'a [u8], p: &mut usize) -> Result<&'a [u8]> {
        let start = *p;
        while d.get(*p).is_some_and(u8::is_ascii_digit) {
            *p += 1;
        }
        if *p == start || d.get(*p) != Some(&b':') || (*p - start > 1 && d[start] == b'0') {
            return Err(bad());
        }
        let len = std::str::from_utf8(&d[start..*p])
            .map_err(|_| bad())?
            .parse::<usize>()
            .map_err(|_| bad())?;
        *p += 1;
        let end = p
            .checked_add(len)
            .filter(|n| *n <= d.len())
            .ok_or_else(bad)?;
        let s = &d[*p..end];
        *p = end;
        Ok(s)
    }
    fn value(d: &[u8], p: &mut usize, depth: u8) -> Result<()> {
        if depth > 32 {
            return Err(bad());
        }
        match d.get(*p) {
            Some(b'0'..=b'9') => {
                string(d, p)?;
            }
            Some(b'i') => {
                *p += 1;
                let start = *p;
                while d.get(*p).is_some_and(|c| *c != b'e') {
                    *p += 1;
                }
                let s =
                    std::str::from_utf8(d.get(start..*p).ok_or_else(bad)?).map_err(|_| bad())?;
                let n = s.parse::<i64>().map_err(|_| bad())?;
                if s != n.to_string() {
                    return Err(bad());
                }
                if d.get(*p) != Some(&b'e') {
                    return Err(bad());
                }
                *p += 1;
            }
            Some(b'l' | b'd') => {
                let dict = d[*p] == b'd';
                *p += 1;
                let mut prev: Option<&[u8]> = None;
                while d.get(*p) != Some(&b'e') {
                    if dict {
                        let k = string(d, p)?;
                        if prev.is_some_and(|s| s >= k) {
                            return Err(bad());
                        }
                        prev = Some(k);
                    }
                    value(d, p, depth + 1)?;
                }
                *p += 1;
            }
            _ => return Err(bad()),
        }
        Ok(())
    }
    if data.first() != Some(&b'd') {
        return Err(bad());
    }
    let mut p = 1;
    let mut info = None;
    let mut prev: Option<&[u8]> = None;
    while data.get(p) != Some(&b'e') {
        let k = string(data, &mut p)?;
        if prev.is_some_and(|s| s >= k) {
            return Err(bad());
        }
        prev = Some(k);
        let start = p;
        value(data, &mut p, 0)?;
        if k == b"info" {
            if data[start] != b'd' {
                return Err(bad());
            }
            info = Some(&data[start..p]);
        }
    }
    if p + 1 != data.len() {
        return Err(bad());
    }
    let info = info.ok_or_else(bad)?;
    // v2-only torrents need a different hash algorithm and are deliberately rejected.
    let mut p = 1;
    let mut has_pieces = false;
    while info.get(p) != Some(&b'e') {
        let key = string(info, &mut p)?;
        if key == b"pieces" {
            let pieces = string(info, &mut p)?;
            if pieces.len() % 20 != 0 {
                return Err(bad());
            }
            has_pieces = true;
        } else {
            value(info, &mut p, 0)?;
        }
    }
    if !has_pieces {
        return Err(bad());
    }
    Ok(hex::encode(Sha1::digest(info)))
}
