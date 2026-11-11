//! Web seed URLs (BEP 19, "GetRight style"): the `url-list` key of a .torrent
//! file and the `ws` parameter of a magnet link.
//!
//! The Hoffman style HTTP seeds of BEP 17 (`httpseeds`) are a different
//! protocol and are not supported: that key is ignored.

use std::fmt;

use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// The most web seeds kept for one torrent. Anything beyond is ignored.
pub const MAX_WEB_SEEDS: usize = 64;

/// The longest web seed URL accepted, in bytes.
pub const MAX_WEB_SEED_URL_LEN: usize = 4096;

/// Whether `url` can be used as a web seed: an absolute `http` or `https` URL
/// with a host, at most [`MAX_WEB_SEED_URL_LEN`] bytes long.
pub fn is_web_seed_url(url: &str) -> bool {
    if url.len() > MAX_WEB_SEED_URL_LEN {
        return false;
    }
    match url::Url::parse(url) {
        Ok(u) => matches!(u.scheme(), "http" | "https") && u.host().is_some(),
        Err(_) => false,
    }
}

/// Appends `url` to `list` if it is a usable web seed URL (see
/// [`is_web_seed_url`]), not in the list yet, and the list has fewer than
/// [`MAX_WEB_SEEDS`] entries. Returns whether it was appended.
pub fn push_web_seed(list: &mut Vec<String>, url: &str) -> bool {
    let url = url.trim();
    if list.len() >= MAX_WEB_SEEDS || !is_web_seed_url(url) || list.iter().any(|u| u == url) {
        return false;
    }
    list.push(url.to_owned());
    true
}

/// Deserializes `url-list`: a single string or a list of strings. Entries that
/// are not usable web seed URLs (not UTF-8, not http(s), not strings at all)
/// are skipped, duplicates dropped and the count bounded by [`MAX_WEB_SEEDS`].
/// A value of any other type yields an empty list rather than an error, so a
/// malformed `url-list` never makes the torrent unreadable.
pub fn deserialize_url_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(UrlListVisitor)
}

struct UrlListVisitor;

impl<'de> Visitor<'de> for UrlListVisitor {
    type Value = Vec<String>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a URL or a list of URLs")
    }

    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        let mut list = Vec::new();
        if let Ok(s) = std::str::from_utf8(v) {
            push_web_seed(&mut list, s);
        }
        Ok(list)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        self.visit_bytes(v.as_bytes())
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }

    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(Vec::new())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut list = Vec::new();
        // Every element is consumed, the ones past the bound too, so the
        // parser stays in step.
        while let Some(entry) = seq.next_element::<UrlListEntry>()? {
            if let UrlListEntry(Some(url)) = entry {
                push_web_seed(&mut list, &url);
            }
        }
        Ok(list)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Vec::new())
    }
}

// One element of a `url-list` list: a UTF-8 string, or anything else (None).
struct UrlListEntry(Option<String>);

impl<'de> serde::Deserialize<'de> for UrlListEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = UrlListEntry;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a URL")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                // Longer than any URL we would keep: not copied.
                if v.len() > MAX_WEB_SEED_URL_LEN {
                    return Ok(UrlListEntry(None));
                }
                Ok(UrlListEntry(
                    std::str::from_utf8(v).ok().map(|s| s.to_owned()),
                ))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                self.visit_bytes(v.as_bytes())
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(UrlListEntry(None))
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(UrlListEntry(None))
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(UrlListEntry(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(UrlListEntry(None))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(UrlListEntry(None))
            }
        }

        deserializer.deserialize_any(EntryVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_web_seed_url() {
        assert!(is_web_seed_url("http://example.com/file"));
        assert!(is_web_seed_url("https://example.com/dir/"));
        assert!(is_web_seed_url("http://127.0.0.1:8080/x"));
        assert!(!is_web_seed_url("ftp://example.com/file"));
        assert!(!is_web_seed_url("udp://tracker:80/announce"));
        assert!(!is_web_seed_url("example.com/file"));
        assert!(!is_web_seed_url(""));
        assert!(!is_web_seed_url("http://"));
        let long = format!("http://example.com/{}", "a".repeat(MAX_WEB_SEED_URL_LEN));
        assert!(!is_web_seed_url(&long));
    }

    #[test]
    fn test_push_web_seed_dedupes_and_bounds() {
        let mut list = Vec::new();
        assert!(push_web_seed(&mut list, "http://a/x"));
        assert!(!push_web_seed(&mut list, "http://a/x"));
        assert!(push_web_seed(&mut list, " http://b/x "));
        assert_eq!(list, vec!["http://a/x", "http://b/x"]);
        for i in 0..MAX_WEB_SEEDS * 2 {
            push_web_seed(&mut list, &format!("http://h{i}/"));
        }
        assert_eq!(list.len(), MAX_WEB_SEEDS);
    }
}
