use std::str::FromStr;

use anyhow::Context;

use crate::hash_id::{Id20, Id32};

/// A parsed magnet link.
pub struct Magnet {
    id20: Option<Id20>,
    id32: Option<Id32>,
    pub trackers: Vec<String>,
    select_only: Option<Vec<usize>>
}

impl Magnet {
    pub fn as_id20(&self) -> Option<Id20> {
        self.id20
    }

    pub fn as_id32(&self) -> Option<Id32> {
        self.id32
    }
    pub fn get_select_only(&self) -> Option<Vec<usize>> {
        self.select_only.clone()
    }

    pub fn from_id20(id20: Id20, trackers: Vec<String>, select_only: Option<Vec<usize>> ) -> Self {
        Self {
            id20: Some(id20),
            id32: None,
            trackers,
            select_only,
        }
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
        let mut files = Vec::<usize>::new();
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
                "so" => {
                    // Process 'so' values, but silently ignore any which fail parsing
                    for file_desc in value.split(',') {
                        if file_desc.is_empty() {
                            continue;
                        }
                        // Handling ranges of file indices
                        if let Some((start, end)) = file_desc.split_once('-') {
                            let maybe_start_idx: Result<usize, _> = start.parse();
                            let maybe_end_idx: Result<usize, _> = end.parse();
                            if let (Ok(start_idx), Ok(end_idx)) = (maybe_start_idx, maybe_end_idx) {
                                files.extend(start_idx..=end_idx);
                            }
                        } else {
                            // Handling single file index
                            let idx = file_desc.parse();
                            if let Ok(idx) = idx {
                                files.push(idx);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        match info_hash_found {
            true => Ok(Magnet {
                id20,
                id32,
                trackers,
                select_only: match files.is_empty() {
                    true => None,
                    false => Some(files),
                },
            }),
            false => {
                anyhow::bail!("did not find infohash")
            }
        }
    }
}

impl std::fmt::Display for Magnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "magnet:")?;
        let mut write_ampersand = {
            let mut written_so_far = 0;
            move |f: &mut std::fmt::Formatter<'_>| -> core::fmt::Result {
                if written_so_far == 0 {
                    write!(f, "?")?;
                } else {
                    write!(f, "&")?;
                }
                written_so_far += 1;
                Ok(())
            }
        };
        if let Some(id20) = self.id20 {
            write_ampersand(f)?;
            write!(f, "xt=urn:btih:{}", id20.as_string(),)?;
        }
        if let Some(id32) = self.id32 {
            write_ampersand(f)?;
            write!(f, "xt=xt=urn:btmh:1220{}", id32.as_string(),)?;
        }
        for tracker in self.trackers.iter() {
            write_ampersand(f)?;
            write!(f, "tr={tracker}")?;
        }
        if let Some(select_only) = &self.select_only {
            if !select_only.is_empty() {
                write_ampersand(f)?;
                write!(f, "so=")?;
                for (index, file) in select_only.iter().enumerate() {
                    if index > 0 {
                        write!(f, ",")?; // Add a comma before all but the first index
                    }
                    write!(f, "{}", file)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::Id20;

    use super::Magnet;

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

    #[test]
    fn test_magnet_to_string() {
        let id20 = Id20::from_str("a621779b5e3d486e127c3efbca9b6f8d135f52e5").unwrap();
        assert_eq!(
            &Magnet::from_id20(id20, Default::default(), None).to_string(),
            "magnet:?xt=urn:btih:a621779b5e3d486e127c3efbca9b6f8d135f52e5"
        );

        assert_eq!(
            &Magnet::from_id20(id20, vec!["foo".to_string(), "bar".to_string()], None).to_string(),
            "magnet:?xt=urn:btih:a621779b5e3d486e127c3efbca9b6f8d135f52e5&tr=foo&tr=bar"
        );

        assert_eq!(
            &Magnet::from_id20(id20, Default::default(), Some(vec![1,2,3])).to_string(),
            "magnet:?xt=urn:btih:a621779b5e3d486e127c3efbca9b6f8d135f52e5&so=1,2,3"
        );

    }
}
