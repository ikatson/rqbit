use std::{
    collections::{
        HashMap,
        hash_map::Entry::{Occupied, Vacant},
    },
    sync::Arc,
};

use crate::{ManagedTorrentShared, Session, session::TorrentId, torrent_state::TorrentMetadata};

#[derive(Clone)]
pub struct UpnpServerSessionAdapter {
    session: Arc<Session>,
}

use anyhow::Context;
use buffers::ByteBufOwned;
use itertools::Itertools;
use librqbit_core::torrent_metainfo::ValidatedTorrentMetaV1Info;
use tracing::{debug, trace, warn};
use upnp_serve::{
    UpnpServer, UpnpServerOptions,
    services::content_directory::{
        ContentDirectoryBrowseProvider,
        browse::response::{Container, Item, ItemOrContainer},
    },
};

#[derive(Debug, PartialEq, Eq)]
struct TorrentFileTreeNode {
    title: String,
    // must be set for all nodes except the root node.
    parent_id: Option<usize>,
    children: Vec<usize>,

    real_torrent_file_id: Option<usize>,
}

fn encode_id(local_id: usize, torrent_id: usize) -> usize {
    (local_id << 16) | (torrent_id + 1)
}

fn decode_id(id: usize) -> anyhow::Result<(usize, usize)> {
    let torrent_id = id & 0xffff;
    if torrent_id == 0 {
        anyhow::bail!("invalid id")
    }
    let torrent_id = torrent_id - 1;
    Ok((id >> 16, torrent_id))
}

impl TorrentFileTreeNode {
    fn as_item_or_container(
        &self,
        id: usize,
        http_host: &str,
        torrent: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> ItemOrContainer {
        let encoded_id = encode_id(id, torrent.id);
        let encoded_parent_id = self.parent_id.map(|p| encode_id(p, torrent.id));
        match self.real_torrent_file_id {
            Some(fid) => {
                let fi = &metadata.file_infos[fid];
                let filename = &fi.relative_filename;
                // Torrent path joined with "/"
                let last_url_bit = metadata
                    .info
                    .iter_file_details()
                    .nth(fid)
                    .map(|fd| fd.filename.to_vec())
                    .map(|components| {
                        components
                            .into_iter()
                            .map(|c| urlencoding::encode(&c).into_owned())
                            .join("/")
                    })
                    .unwrap_or_else(|| self.title.clone());
                ItemOrContainer::Item(Item {
                    id: encoded_id,
                    parent_id: encoded_parent_id.unwrap_or_default(),
                    title: self.title.clone(),
                    mime_type: mime_guess::from_path(filename).first(),
                    url: format!(
                        "http://{}/torrents/{}/stream/{}/{}",
                        http_host, torrent.id, fid, last_url_bit
                    ),
                    size: fi.len,
                })
            }
            None => ItemOrContainer::Container(Container {
                id: encoded_id,
                parent_id: Some(encoded_parent_id.unwrap_or_default()),
                title: self.title.clone(),
                children_count: Some(self.children.len()),
            }),
        }
    }
}

struct TorrentFileTree {
    // root id is 0
    nodes: Vec<TorrentFileTreeNode>,
}

fn is_single_file_at_root(info: &ValidatedTorrentMetaV1Info<ByteBufOwned>) -> bool {
    info.iter_file_details()
        .flat_map(move |fd| fd.filename.to_vec())
        .nth(1)
        .is_none()
}

impl TorrentFileTree {
    fn build(
        torent_id: TorrentId,
        info: &ValidatedTorrentMetaV1Info<ByteBufOwned>,
    ) -> anyhow::Result<Self> {
        if is_single_file_at_root(info) {
            let filename = info
                .iter_file_details()
                .next()
                .unwrap()
                .filename
                .iter_components()
                .last()
                .unwrap();
            let root_node = TorrentFileTreeNode {
                title: filename.into_owned(),
                parent_id: None,
                children: vec![],
                real_torrent_file_id: Some(0),
            };
            return Ok(TorrentFileTree {
                nodes: vec![root_node],
            });
        }

        let root_node = TorrentFileTreeNode {
            title: info
                .name_or_else(|| format!("torrent {torent_id}"))
                .into_owned(),
            parent_id: None,
            children: vec![],
            real_torrent_file_id: None,
        };

        let mut tree = TorrentFileTree {
            nodes: vec![root_node],
        };

        let mut name_cache = HashMap::new();

        for (fid, fd) in info.iter_file_details().enumerate() {
            let components = fd.filename.to_vec();
            let mut parent_id = 0;
            let mut it = components.iter().peekable();
            while let Some(component) = it.next() {
                let is_last = it.peek().is_none();
                if is_last {
                    let current_id = tree.nodes.len();
                    let node = TorrentFileTreeNode {
                        title: component.clone(),
                        parent_id: Some(parent_id),
                        children: vec![],
                        real_torrent_file_id: Some(fid),
                    };
                    tree.nodes.push(node);
                    tree.nodes[parent_id].children.push(current_id);
                    break;
                }

                parent_id = match name_cache.entry((parent_id, component.clone())) {
                    Occupied(occ) => *occ.get(),
                    Vacant(vac) => {
                        let id = tree.nodes.len();
                        let node = TorrentFileTreeNode {
                            title: component.clone(),
                            parent_id: Some(parent_id),
                            children: vec![],
                            real_torrent_file_id: None,
                        };
                        tree.nodes.push(node);
                        tree.nodes[parent_id].children.push(id);
                        vac.insert(id);
                        id
                    }
                };
            }
        }

        Ok(tree)
    }
}

impl UpnpServerSessionAdapter {
    fn build_root(&self, hostname: &str) -> Vec<ItemOrContainer> {
        let mut all = self
            .session
            .with_torrents(|torrents| torrents.map(|(_, t)| t.clone()).collect_vec());

        all.sort_unstable_by_key(|t| t.id());

        all.iter()
            .filter_map(|t| {
                let real_id = t.id();
                let upnp_id = real_id + 1;
                let metadata = t.metadata.load();
                let metadata = match metadata.as_ref() {
                    Some(r) => r,
                    None => return None,
                };

                if is_single_file_at_root(&metadata.info) {
                    // Just add the file directly
                    let rf = &metadata.file_infos[0].relative_filename;
                    let title = rf.file_name()?.to_str()?.to_owned();
                    Some(
                        TorrentFileTreeNode {
                            title,
                            parent_id: None,
                            children: vec![],
                            real_torrent_file_id: Some(0),
                        }
                        .as_item_or_container(
                            0,
                            hostname,
                            t.shared(),
                            metadata,
                        ),
                    )
                } else {
                    let title = metadata
                        .info
                        .name_or_else(|| format!("torrent {real_id}"))
                        .into_owned();

                    // Create a folder
                    Some(ItemOrContainer::Container(Container {
                        id: upnp_id,
                        parent_id: Some(0),
                        title,
                        children_count: None,
                    }))
                }
            })
            .collect_vec()
    }

    fn build_impl(
        &self,
        object_id: usize,
        http_hostname: &str,
        metadata: bool,
    ) -> Vec<ItemOrContainer> {
        if object_id == 0 {
            let root = self.build_root(http_hostname);
            if metadata {
                return vec![ItemOrContainer::Container(Container {
                    id: 0,
                    parent_id: None,
                    children_count: Some(root.len()),
                    title: "root".to_owned(),
                })];
            }
            return root;
        }

        let (node_id, torrent_id) = match decode_id(object_id) {
            Ok((node_id, torrent_id)) => (node_id, torrent_id),
            Err(_) => {
                debug!(id=?object_id, "invalid id");
                return vec![];
            }
        };
        trace!(object_id, node_id, torrent_id);

        let torrent = match self.session.get(torrent_id.into()) {
            Some(t) => t,
            None => {
                warn!(torrent_id, "no such torrent");
                return vec![];
            }
        };

        let t_metadata = torrent.metadata.load();
        let t_metadata = match t_metadata.as_ref() {
            Some(r) => r,
            None => return vec![],
        };

        let tree = match TorrentFileTree::build(torrent.id(), &t_metadata.info) {
            Ok(tree) => tree,
            Err(e) => {
                warn!(object_id, error=?e, "error building torrent file tree");
                return vec![];
            }
        };

        let node = match tree.nodes.get(node_id) {
            Some(n) => n,
            None => {
                warn!(torrent_id, node_id, "no such internal ID in torrent");
                return vec![];
            }
        };

        trace!(node_id, torrent_id, ?node);

        let mut result = Vec::new();

        if node.real_torrent_file_id.is_some() || metadata {
            result.push(node.as_item_or_container(
                node_id,
                http_hostname,
                torrent.shared(),
                t_metadata,
            ))
        } else {
            for (child_node_id, child_node) in node
                .children
                .iter()
                .filter_map(|id| Some((*id, tree.nodes.get(*id)?)))
            {
                result.push(child_node.as_item_or_container(
                    child_node_id,
                    http_hostname,
                    torrent.shared(),
                    t_metadata,
                ));
            }
        };

        result
    }
}

impl ContentDirectoryBrowseProvider for UpnpServerSessionAdapter {
    fn browse_direct_children(
        &self,
        object_id: usize,
        http_hostname: &str,
    ) -> Vec<ItemOrContainer> {
        self.build_impl(object_id, http_hostname, false)
    }

    fn browse_metadata(&self, object_id: usize, http_hostname: &str) -> Vec<ItemOrContainer> {
        self.build_impl(object_id, http_hostname, true)
    }
}

impl Session {
    pub async fn make_upnp_adapter(
        self: &Arc<Self>,
        friendly_name: String,
        http_listen_port: u16,
    ) -> anyhow::Result<UpnpServer> {
        UpnpServer::new(UpnpServerOptions {
            friendly_name,
            http_listen_port,
            http_prefix: "/upnp".to_owned(),
            browse_provider: Box::new(UpnpServerSessionAdapter {
                session: self.clone(),
            }),
            cancellation_token: self.cancellation_token().child_token(),
        })
        .await
        .context("error creating upnp adapter")
    }
}

#[cfg(test)]
mod tests {
    use bencode::bencode_serialize_to_writer;
    use bytes::Bytes;
    use dht::Id20;
    use librqbit_core::torrent_metainfo::{
        TorrentMetaV1File, TorrentMetaV1Info, TorrentMetaV1Owned,
    };
    use tempfile::TempDir;
    use upnp_serve::services::content_directory::{
        ContentDirectoryBrowseProvider,
        browse::response::{Container, Item, ItemOrContainer},
    };

    use crate::{
        AddTorrent, AddTorrentOptions, Session, SessionOptions,
        tests::test_util::setup_test_logging,
        upnp_server_adapter::{
            TorrentFileTree, TorrentFileTreeNode, UpnpServerSessionAdapter, decode_id, encode_id,
        },
    };

    fn create_torrent(name: Option<&str>, files: &[&str]) -> TorrentMetaV1Owned {
        TorrentMetaV1Owned {
            announce: None,
            announce_list: vec![],
            info: bencode::WithRawBytes {
                data: TorrentMetaV1Info {
                    name: name.map(|n| n.as_bytes().into()),
                    pieces: Some(b""[..].into()),
                    piece_length: 1,
                    length: None,
                    md5sum: None,
                    files: Some(
                        files
                            .iter()
                            .map(|f| TorrentMetaV1File {
                                length: 1,
                                path: f.split("/").map(|f| f.as_bytes().into()).collect(),
                                attr: None,
                                sha1: None,
                                symlink_path: None,
                            })
                            .collect(),
                    ),
                    attr: None,
                    sha1: None,
                    symlink_path: None,
                    private: false,
                    meta_version: None,
                    file_tree: None,
                },
                raw_bytes: Default::default(),
            },
            comment: None,
            created_by: None,
            encoding: None,
            publisher: None,
            publisher_url: None,
            creation_date: None,
            info_hash: Id20::default(),
            info_hash_v2: None,
            piece_layers: None,
        }
    }

    #[test]
    fn test_torrent_file_tree_single() -> anyhow::Result<()> {
        let t = create_torrent(Some("test t"), &["file0"]);
        let tree = TorrentFileTree::build(0, &t.info.data.validate().unwrap())?;
        assert_eq!(
            &tree.nodes,
            &[TorrentFileTreeNode {
                children: vec![],
                parent_id: None,
                real_torrent_file_id: Some(0),
                title: "file0".into()
            }]
        );

        Ok(())
    }

    #[test]
    fn test_torrent_file_tree_flat() -> anyhow::Result<()> {
        let t = create_torrent(Some("test t"), &["file0", "file1"]);
        let tree = TorrentFileTree::build(0, &t.info.data.validate().unwrap())?;
        assert_eq!(
            &tree.nodes,
            &[
                TorrentFileTreeNode {
                    children: vec![1, 2],
                    parent_id: None,
                    real_torrent_file_id: None,
                    title: "test t".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(0),
                    real_torrent_file_id: Some(0),
                    title: "file0".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(0),
                    real_torrent_file_id: Some(1),
                    title: "file1".into()
                }
            ]
        );

        Ok(())
    }

    #[test]
    fn test_torrent_file_tree_nested() -> anyhow::Result<()> {
        let t = create_torrent(
            Some("test t"),
            &["file0", "file1", "dir0/file2", "dir0/dir1/file3"],
        );
        let tree = TorrentFileTree::build(0, &t.info.data.validate().unwrap())?;
        assert_eq!(
            &tree.nodes,
            &[
                TorrentFileTreeNode {
                    children: vec![1, 2, 3],
                    parent_id: None,
                    real_torrent_file_id: None,
                    title: "test t".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(0),
                    real_torrent_file_id: Some(0),
                    title: "file0".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(0),
                    real_torrent_file_id: Some(1),
                    title: "file1".into()
                },
                TorrentFileTreeNode {
                    children: vec![4, 5],
                    parent_id: Some(0),
                    real_torrent_file_id: None,
                    title: "dir0".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(3),
                    real_torrent_file_id: Some(2),
                    title: "file2".into()
                },
                TorrentFileTreeNode {
                    children: vec![6],
                    parent_id: Some(3),
                    real_torrent_file_id: None,
                    title: "dir1".into()
                },
                TorrentFileTreeNode {
                    children: vec![],
                    parent_id: Some(5),
                    real_torrent_file_id: Some(3),
                    title: "file3".into()
                },
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_browse() {
        setup_test_logging();

        let t1 = create_torrent(Some("t1"), &["f1"]);
        let t2 = create_torrent(Some("t2"), &["d1/f2"]);

        fn as_bytes(t: &TorrentMetaV1Owned) -> Bytes {
            let mut b = Vec::new();
            bencode_serialize_to_writer(t, &mut b).unwrap();
            b.into()
        }

        let td = TempDir::new().unwrap();
        let session = Session::new_with_opts(
            td.path().to_owned(),
            SessionOptions {
                disable_dht: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        session
            .add_torrent(
                AddTorrent::from_bytes(as_bytes(&t1)),
                Some(AddTorrentOptions {
                    paused: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        session
            .add_torrent(
                AddTorrent::from_bytes(as_bytes(&t2)),
                Some(AddTorrentOptions {
                    paused: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let adapter = UpnpServerSessionAdapter { session };

        assert_eq!(
            adapter.browse_metadata(0, "127.0.0.1"),
            vec![ItemOrContainer::Container(Container {
                id: 0,
                parent_id: None,
                children_count: Some(2),
                title: "root".into()
            })]
        );

        assert_eq!(
            adapter.browse_direct_children(0, "127.0.0.1"),
            vec![
                ItemOrContainer::Item(Item {
                    id: encode_id(0, 0),
                    parent_id: 0,
                    title: "f1".into(),
                    mime_type: None,
                    url: "http://127.0.0.1/torrents/0/stream/0/f1".into(),
                    size: 1,
                }),
                ItemOrContainer::Container(Container {
                    id: encode_id(0, 1),
                    parent_id: Some(0),
                    children_count: None,
                    title: "t2".into()
                })
            ]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(0, 0), "127.0.0.1"),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(0, 0),
                parent_id: 0,
                title: "f1".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/0/stream/0/f1".into(),
                size: 1,
            })]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(0, 1), "127.0.0.1"),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(0, 1),
                parent_id: Some(0),
                children_count: Some(1),
                title: "t2".into()
            })]
        );

        assert_eq!(
            adapter.browse_direct_children(encode_id(0, 1), "127.0.0.1"),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(1, 1),
                parent_id: Some(encode_id(0, 1)),
                children_count: Some(1),
                title: "d1".into()
            }),]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(1, 1), "127.0.0.1"),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(1, 1),
                parent_id: Some(encode_id(0, 1)),
                children_count: Some(1),
                title: "d1".into()
            }),]
        );

        assert_eq!(
            adapter.browse_direct_children(encode_id(1, 1), "127.0.0.1"),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(2, 1),
                parent_id: encode_id(1, 1),
                title: "f2".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/1/stream/0/d1/f2".into(),
                size: 1,
            })]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(2, 1), "127.0.0.1"),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(2, 1),
                parent_id: encode_id(1, 1),
                title: "f2".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/1/stream/0/d1/f2".into(),
                size: 1,
            })]
        );
    }

    #[test]
    fn test_encode_id() {
        for local_id in 0..5 {
            for torrent_id in 0..5 {
                let encoded = encode_id(local_id, torrent_id);
                let (decoded_local_id, decoded_torrent_id) = decode_id(encoded).unwrap();
                assert_eq!(local_id, decoded_local_id);
                assert_eq!(torrent_id, decoded_torrent_id);
            }
        }
    }
}
