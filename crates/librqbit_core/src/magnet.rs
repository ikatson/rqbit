use std::str::FromStr;

use anyhow::Context;

use crate::id20::Id20;

pub struct Magnet {
    pub info_hash: Id20,
    pub trackers: Vec<String>,
}

impl Magnet {
    pub fn parse(url: &str) -> anyhow::Result<Magnet> {
        let url = url::Url::parse(url).context("magnet link must be a valid URL")?;
        if url.scheme() != "magnet" {
            anyhow::bail!("expected scheme magnet");
        }
        let mut info_hash: Option<Id20> = None;
        let mut trackers = Vec::<String>::new();
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "xt" => match value.as_ref().strip_prefix("urn:btih:") {
                    Some(infohash) => {
                        info_hash.replace(Id20::from_str(infohash)?);
                    }
                    None => anyhow::bail!("expected xt to start with urn:btih:"),
                },
                "tr" => trackers.push(value.into()),
                _ => {}
            }
        }
        match info_hash {
            Some(info_hash) => Ok(Magnet {
                info_hash,
                trackers,
            }),
            None => {
                anyhow::bail!("did not find infohash")
            }
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
}
