pub mod content_directory {
    use response::ItemOrContainer;

    pub mod request {
        use anyhow::Context;
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Envelope {
            #[serde(rename = "Body")]
            body: Body,
        }

        #[derive(Deserialize)]
        struct Body {
            #[serde(rename = "Browse")]
            browse: ContentDirectoryControlRequest,
        }

        #[derive(Deserialize, PartialEq, Eq, Debug)]
        pub enum BrowseFlag {
            BrowseDirectChildren,
            BrowseMetadata,
        }

        #[derive(Deserialize, Debug)]
        pub struct ContentDirectoryControlRequest {
            #[serde(rename = "ObjectID")]
            pub object_id: usize,
            #[serde(rename = "BrowseFlag")]
            pub browse_flag: BrowseFlag,
            #[serde(rename = "StartingIndex", default)]
            pub starting_index: usize,
            #[serde(rename = "RequestedCount", default)]
            pub requested_count: usize,
        }

        impl ContentDirectoryControlRequest {
            pub fn parse(s: &str) -> anyhow::Result<Self> {
                let envelope: Envelope =
                    quick_xml::de::from_str(s).context("error deserializing")?;
                Ok(envelope.body.browse)
            }
        }
    }

    pub mod response {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Container {
            pub id: usize,
            pub parent_id: Option<usize>,
            pub children_count: Option<usize>,
            pub title: String,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Item {
            pub id: usize,
            pub parent_id: Option<usize>,
            pub title: String,
            pub mime_type: Option<mime_guess::Mime>,
            pub url: String,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
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
    use crate::upnp_types::content_directory::request::{
        BrowseFlag, ContentDirectoryControlRequest,
    };

    #[test]
    fn test_parse_content_directory_request() {
        let s = include_str!("resources/test/ContentDirectoryControlExampleRequest.xml");
        let req = ContentDirectoryControlRequest::parse(s).unwrap();
        assert_eq!(req.object_id, 5);
        assert_eq!(req.browse_flag, BrowseFlag::BrowseDirectChildren)
    }
}
