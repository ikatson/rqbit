pub struct ContentDirectoryControlRequest {
    pub object_id: usize,
}

impl ContentDirectoryControlRequest {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let mut reader = quick_xml::Reader::from_str(s);

        use quick_xml::events::Event::{Eof, Start};

        let mut object_id: Option<usize> = None;

        loop {
            match reader.read_event()? {
                Eof => break,
                Start(e) if e.name().as_ref() == b"ObjectID" => {
                    let t = reader.read_text(e.to_end().name())?;
                    object_id = t.trim().parse().ok();
                }
                _ => continue,
            }
        }

        Ok(ContentDirectoryControlRequest {
            object_id: object_id.unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ContentDirectoryControlRequest;

    #[test]
    fn test_parse_content_directory_request() {
        let s = include_str!("resources/debug/ContentDirectoryControlExampleRequest.xml");
        let req = ContentDirectoryControlRequest::parse(s).unwrap();
        assert_eq!(req.object_id, 5);
    }
}
