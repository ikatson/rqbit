//! BEP 48: scraping HTTP trackers.

use std::collections::HashMap;

use anyhow::{Context, bail};
use bencode::{BencodeValue, ByteBuf};
use librqbit_core::hash_id::Id20;
use url::Url;

/// What a tracker knows about a torrent. Fields the tracker leaves out stay None.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScrapeCounts {
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub downloaded: Option<u32>,
}

impl ScrapeCounts {
    /// The larger of every field; used for a tracker that is reached over two addresses.
    pub fn max(self, other: Self) -> Self {
        Self {
            seeders: self.seeders.max(other.seeders),
            leechers: self.leechers.max(other.leechers),
            downloaded: self.downloaded.max(other.downloaded),
        }
    }
}

const ANNOUNCE: &str = "announce";
const SCRAPE: &str = "scrape";

/// Whether the tracker can be scraped: the last path segment of its announce URL has to
/// start with `announce` (BEP 48).
pub(crate) fn is_scrapable(announce: &Url) -> bool {
    announce
        .path()
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with(ANNOUNCE))
}

/// The scrape URL of a tracker (BEP 48): `announce` in the last path segment becomes
/// `scrape`, everything after it (`.php`, ...) and the query string stay, and the
/// `info_hash` is added to the query. None when the tracker can't be scraped.
pub(crate) fn scrape_url(announce: &Url, info_hash: &Id20) -> Option<Url> {
    let path = announce.path();
    let (dir, last) = path.rsplit_once('/')?;
    let rest = last.strip_prefix(ANNOUNCE)?;
    let mut url = announce.clone();
    url.set_path(&format!("{dir}/{SCRAPE}{rest}"));
    let hash = urlencoding::encode_binary(info_hash.0.as_ref());
    let query = match announce.query() {
        Some(q) if !q.is_empty() => format!("{q}&info_hash={hash}"),
        _ => format!("info_hash={hash}"),
    };
    url.set_query(Some(&query));
    Some(url)
}

fn dict_get<'a, 'b>(
    dict: &'b HashMap<ByteBuf<'a>, BencodeValue<ByteBuf<'a>>>,
    key: &[u8],
) -> Option<&'b BencodeValue<ByteBuf<'a>>> {
    dict.iter().find(|(k, _)| k.as_ref() == key).map(|(_, v)| v)
}

fn count(value: Option<&BencodeValue<ByteBuf<'_>>>) -> Option<u32> {
    match value {
        Some(BencodeValue::Integer(i)) => Some(u32::try_from((*i).max(0)).unwrap_or(u32::MAX)),
        _ => None,
    }
}

/// Reads the answer of a tracker to a scrape of `info_hash`. A tracker that does not know
/// the torrent or leaves keys out gives empty counts; a `failure reason` is an error.
pub(crate) fn parse_scrape(bytes: &[u8], info_hash: &Id20) -> anyhow::Result<ScrapeCounts> {
    let value = bencode::dyn_from_bytes::<ByteBuf<'_>>(bytes)
        .map_err(|e| e.into_kind())
        .context("not a bencoded response")?;
    let BencodeValue::Dict(root) = value else {
        bail!("the response is not a dictionary");
    };
    if let Some(BencodeValue::Bytes(reason)) = dict_get(&root, b"failure reason") {
        bail!(
            "tracker returned failure. Failure reason: {}",
            String::from_utf8_lossy(reason.as_ref())
        );
    }
    let Some(BencodeValue::Dict(files)) = dict_get(&root, b"files") else {
        return Ok(ScrapeCounts::default());
    };
    let Some(BencodeValue::Dict(file)) = dict_get(files, info_hash.0.as_ref()) else {
        return Ok(ScrapeCounts::default());
    };
    Ok(ScrapeCounts {
        seeders: count(dict_get(file, b"complete")),
        leechers: count(dict_get(file, b"incomplete")),
        downloaded: count(dict_get(file, b"downloaded")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> Id20 {
        Id20::new(*b"xxxxxxxxxxxxxxxxxxxx")
    }

    #[test]
    fn scrape_url_derivation() {
        let h = "info_hash=xxxxxxxxxxxxxxxxxxxx";
        let cases: &[(&str, Option<String>)] = &[
            (
                "http://t.example/announce",
                Some(format!("http://t.example/scrape?{h}")),
            ),
            (
                "http://t.example:6969/announce",
                Some(format!("http://t.example:6969/scrape?{h}")),
            ),
            (
                "https://t.example/x/y/announce",
                Some(format!("https://t.example/x/y/scrape?{h}")),
            ),
            // What follows `announce` in the segment stays.
            (
                "http://t.example/announce.php",
                Some(format!("http://t.example/scrape.php?{h}")),
            ),
            ("http://t.example/announce/", None),
            (
                "http://t.example/x/announce2",
                Some(format!("http://t.example/x/scrape2?{h}")),
            ),
            // The query string stays and the info hash is appended.
            (
                "http://t.example/announce?passkey=abc",
                Some(format!("http://t.example/scrape?passkey=abc&{h}")),
            ),
            (
                "http://t.example/a/announce.php?x=1&y=2",
                Some(format!("http://t.example/a/scrape.php?x=1&y=2&{h}")),
            ),
            // Only the last segment counts.
            ("http://t.example/announce/abc", None),
            ("http://t.example/", None),
            ("http://t.example", None),
            ("http://t.example/scrape", None),
            ("http://t.example/myannounce", None),
            ("http://t.example/Announce", None),
        ];
        for (input, expected) in cases {
            let url: Url = input.parse().unwrap();
            let got = scrape_url(&url, &hash()).map(|u| u.to_string());
            assert_eq!(&got, expected, "{input}");
            assert_eq!(is_scrapable(&url), expected.is_some(), "{input}");
        }
    }

    #[test]
    fn scrape_url_escapes_the_hash() {
        let url: Url = "http://t.example/announce".parse().unwrap();
        let mut raw = [0u8; 20];
        raw[0] = 0xff;
        raw[1] = b' ';
        raw[2] = b'&';
        let got = scrape_url(&url, &Id20::new(raw)).unwrap();
        assert!(
            got.as_str()
                .starts_with("http://t.example/scrape?info_hash=%FF%20%26%00"),
            "{got}"
        );
    }

    #[test]
    fn parses_the_bep48_example() {
        let mut body = b"d5:filesd20:".to_vec();
        body.extend_from_slice(hash().0.as_ref());
        body.extend_from_slice(b"d8:completei11e10:downloadedi13772e10:incompletei19eeee");
        let c = parse_scrape(&body, &hash()).unwrap();
        assert_eq!(
            c,
            ScrapeCounts {
                seeders: Some(11),
                leechers: Some(19),
                downloaded: Some(13772)
            }
        );
    }

    #[test]
    fn tolerates_missing_keys_and_other_torrents() {
        let mut body = b"d5:filesd20:".to_vec();
        body.extend_from_slice(hash().0.as_ref());
        body.extend_from_slice(b"d8:completei3eeee");
        let c = parse_scrape(&body, &hash()).unwrap();
        assert_eq!((c.seeders, c.leechers, c.downloaded), (Some(3), None, None));

        // The answer is about another torrent.
        let mut other = b"d5:filesd20:".to_vec();
        other.extend_from_slice(b"yyyyyyyyyyyyyyyyyyyy");
        other.extend_from_slice(b"d8:completei3eeee");
        assert_eq!(
            parse_scrape(&other, &hash()).unwrap(),
            ScrapeCounts::default()
        );

        // No files at all, or an empty dictionary.
        assert_eq!(
            parse_scrape(b"de", &hash()).unwrap(),
            ScrapeCounts::default()
        );
        assert_eq!(
            parse_scrape(b"d5:filesdee", &hash()).unwrap(),
            ScrapeCounts::default()
        );
        // A negative number saturates to zero.
        let mut neg = b"d5:filesd20:".to_vec();
        neg.extend_from_slice(hash().0.as_ref());
        neg.extend_from_slice(b"d8:completei-5eeee");
        assert_eq!(parse_scrape(&neg, &hash()).unwrap().seeders, Some(0));
    }

    #[test]
    fn failure_reason_is_an_error() {
        let e = parse_scrape(b"d14:failure reason7:go awaye", &hash()).unwrap_err();
        assert!(format!("{e:#}").contains("go away"), "{e:#}");
    }

    #[test]
    fn malformed_input_is_an_error_and_never_panics() {
        for bad in [
            &b""[..],
            b"garbage",
            b"i5e",
            b"l5:filese",
            b"d5:files",
            b"d5:filesd20:xx",
            b"d5:files5:abcdee",
            b"\xff\xfe\x00",
        ] {
            let r = parse_scrape(bad, &hash());
            // Either an error or empty counts, but no panic.
            if let Ok(c) = r {
                assert_eq!(c, ScrapeCounts::default(), "{bad:?}");
            }
        }
        assert!(parse_scrape(b"", &hash()).is_err());
        assert!(parse_scrape(b"i5e", &hash()).is_err());
    }
}
