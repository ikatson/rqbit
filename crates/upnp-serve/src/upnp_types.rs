pub mod content_directory {
    use response::ItemOrContainer;

    pub mod request {
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
    }

    pub mod response {
        #[derive(Debug, Clone)]
        pub struct Container {
            pub id: usize,
            pub parent_id: Option<usize>,
            pub children_count: Option<usize>,
            pub title: String,
        }

        #[derive(Debug, Clone)]
        pub struct Item {
            pub id: usize,
            pub parent_id: Option<usize>,
            pub title: String,
            pub mime_type: Option<mime_guess::Mime>,
            pub url: String,
        }

        #[derive(Debug, Clone)]
        pub enum ItemOrContainer {
            Container(Container),
            Item(Item),
        }
    }

    pub trait ContentDirectoryBrowseProvider: Send + Sync {
        fn browse_direct_children(&self, parent_id: usize) -> Vec<ItemOrContainer>;
    }

    impl ContentDirectoryBrowseProvider for Vec<ItemOrContainer> {
        fn browse_direct_children(&self, _parent_id: usize) -> Vec<ItemOrContainer> {
            self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::upnp_types::content_directory::request::ContentDirectoryControlRequest;

    #[test]
    fn test_parse_content_directory_request() {
        let s = include_str!("resources/debug/ContentDirectoryControlExampleRequest.xml");
        let req = ContentDirectoryControlRequest::parse(s).unwrap();
        assert_eq!(req.object_id, 5);
    }
}
