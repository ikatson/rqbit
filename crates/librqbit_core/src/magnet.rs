use std::str::FromStr;

use anyhow::Context;

use crate::hash_id::{Id20, Id32};

/// A parsed magnet link.
pub struct Magnet {
    id20: Option<Id20>,
    id32: Option<Id32>,
    pub trackers: Vec<String>,
}

impl Magnet {
    pub fn as_id20(&self) -> Option<Id20> {
        self.id20
    }

    pub fn as_id32(&self) -> Option<Id32> {
        self.id32
    }

    /// Parse a magnet link.
    pub fn parse(url: &str) -> anyhow::Result<Magnet> {
        let url = url::Url::parse(url).context("magnet link must be a valid URL")?;
        if url.scheme() != "magnet" {
            anyhow::bail!("expected scheme magnet");
        }
        let mut info_hash_found = false;
        let mut id20: Option<Id20> = None;
        let mut id32: Option<Id32> = None;
        let mut trackers = Vec::<String>::new();
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "xt" => {
                    if let Some(ih) = value.as_ref().strip_prefix("urn:btih:") {
                        let i = Id20::from_str(ih)?;
                        id20.replace(i);
                        info_hash_found = true;
                    } else if let Some(ih) = value.as_ref().strip_prefix("urn:btmh:1220") {
                        let i = Id32::from_str(ih)?;
                        id32.replace(i);
                        info_hash_found = true;
                    } else {
                        anyhow::bail!("expected xt to start with btih or btmh");
                    }
                }
                "tr" => trackers.push(value.into()),
                _ => {}
            }
        }
        match info_hash_found {
            true => Ok(Magnet {
                id20,
                id32,
                trackers,
            }),
            false => {
                anyhow::bail!("did not find infohash")
            }
        }
    }
}

impl std::fmt::Display for Magnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let (Some(id20), Some(id32)) = (self.id20, self.id32) {
            write!(
                f,
                "magnet:?xt=urn:btih:{}?xt=urn:btmh:1220{}&tr={}",
                id20.as_string(),
                id32.as_string(),
                self.trackers.join("&tr=")
            )
        } else if let Some(id20) = self.id20 {
            write!(
                f,
                "magnet:?xt=urn:btih:{}&tr={}",
                id20.as_string(),
                self.trackers.join("&tr=")
            )
        } else if let Some(id32) = self.id32 {
            write!(
                f,
                "magnet:?xt=urn:btmh:1220{}&tr={}",
                id32.as_string(),
                self.trackers.join("&tr=")
            )
        } else {
            panic!("no infohash")
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_magnet_as_url() {
        let magnet = "magnet:?xt=urn:btih:a621779b5e3d486e127c3efbca9b6f8d135f52e5&dn=rutor.info_%D0%92%D0%BE%D0%B9%D0%BD%D0%B0+%D0%B1%D1%83%D0%B4%D1%83%D1%89%D0%B5%D0%B3%D0%BE+%2F+The+Tomorrow+War+%282021%29+WEB-DLRip+%D0%BE%D1%82+MegaPeer+%7C+P+%7C+NewComers&tr=udp://opentor.org:2710&tr=udp://opentor.org:2710&tr=http://retracker.local/announce";
        dbg!(url::Url::parse(magnet).unwrap());
    }

    #[test]
    fn test_parse_magnet_v2() {
        use super::Magnet;
        use crate::magnet::Id32;
        use std::str::FromStr;
        let magnet = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e&dn=bittorrent-v2-test
";
        let info_hash =
            Id32::from_str("caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e")
                .unwrap();
        let m = Magnet::parse(magnet).unwrap();
        assert!(m.as_id32() == Some(info_hash));
    }
}
