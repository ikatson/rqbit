use std::{
    collections::{
        HashMap,
        hash_map::Entry::{Occupied, Vacant},
    },
    net::IpAddr,
    sync::Arc,
};

use crate::{ManagedTorrentShared, Session, session::TorrentId, torrent_state::TorrentMetadata};

#[derive(Clone)]
pub struct UpnpServerSessionAdapter {
    session: Arc<Session>,
    renderer_capabilities: Arc<dashmap::DashMap<IpAddr, upnp_serve::state::RendererCapabilities>>,
}

use anyhow::Context;
use axum::extract::ConnectInfo;
use buffers::ByteBufOwned;
use itertools::Itertools;
use librqbit_core::torrent_metainfo::ValidatedTorrentMetaV1Info;
use librqbit_dualstack_sockets::WrappedSocketAddr;
use tracing::{debug, trace, warn};
use upnp_serve::{
    UpnpServer, UpnpServerOptions,
    services::content_directory::{
        ContentDirectoryBrowseProvider,
        browse::response::{Container, Item, ItemOrContainer},
    },
    state::RendererCapabilities,
};

// High bit flag to mark all IDs belonging to the "Transcoded" virtual subtree.
// Uses usize::BITS - 2 so it's safe on both 32-bit (armv7) and 64-bit targets.
const TRANSCODED_FLAG: usize = 1 << (usize::BITS - 2);

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

/// What content to show for a given renderer client.
enum TranscodeMode {
    /// TV supports DTS — show originals only, no Transcoded folder.
    OriginalOnly,
    /// TV doesn't support DTS — show transcoded versions only (root IS transcoded view).
    TranscodedOnly,
    /// Unknown TV — show both original and Transcoded folder (safe fallback).
    Both,
}

impl TorrentFileTreeNode {
    fn as_item_or_container(
        &self,
        id: usize,
        http_host: &str,
        torrent: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
        transcoded: bool,
    ) -> ItemOrContainer {
        let encoded_id = encode_id(id, torrent.id);
        let final_id = if transcoded {
            TRANSCODED_FLAG | encoded_id
        } else {
            encoded_id
        };
        let encoded_parent_id = self.parent_id.map(|p| encode_id(p, torrent.id));
        let final_parent_id = encoded_parent_id.map(|p| {
            if transcoded {
                TRANSCODED_FLAG | p
            } else {
                p
            }
        });

        match self.real_torrent_file_id {
            Some(fid) => {
                let fi = &metadata.file_infos[fid];
                let filename = &fi.relative_filename;
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

                let (url, mime_type, seekable) = if transcoded {
                    let url = format!(
                        "http://{}/torrents/{}/transcode/{}/{}",
                        http_host, torrent.id, fid, last_url_bit
                    );
                    let mime: Option<mime_guess::Mime> = "video/mp2t".parse().ok();
                    (url, mime, false)
                } else {
                    let url = format!(
                        "http://{}/torrents/{}/stream/{}/{}",
                        http_host, torrent.id, fid, last_url_bit
                    );
                    let mime = mime_guess::from_path(filename).first();
                    (url, mime, true)
                };

                ItemOrContainer::Item(Item {
                    id: final_id,
                    parent_id: final_parent_id
                        .unwrap_or(if transcoded { TRANSCODED_FLAG } else { 0 }),
                    title: self.title.clone(),
                    mime_type,
                    url,
                    size: fi.len,
                    seekable,
                })
            }
            None => ItemOrContainer::Container(Container {
                id: final_id,
                parent_id: Some(
                    final_parent_id.unwrap_or(if transcoded { TRANSCODED_FLAG } else { 0 }),
                ),
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
        .flat_map(move |fd| fd.filename.iter_components())
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
    fn transcode_mode(&self, client_ip: Option<IpAddr>) -> TranscodeMode {
        let Some(ip) = client_ip else {
            return TranscodeMode::Both;
        };
        match self.renderer_capabilities.get(&ip) {
            Some(caps) if caps.supports_dts => TranscodeMode::OriginalOnly,
            Some(_) => TranscodeMode::TranscodedOnly,
            None => TranscodeMode::Both,
        }
    }

    fn build_root(&self, hostname: &str, transcoded: bool) -> Vec<ItemOrContainer> {
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
                    let rf = &metadata.file_infos[0].relative_filename;
                    let title = rf.file_name()?.to_str()?.to_owned();
                    Some(
                        TorrentFileTreeNode {
                            title,
                            parent_id: None,
                            children: vec![],
                            real_torrent_file_id: Some(0),
                        }
                        .as_item_or_container(0, hostname, t.shared(), metadata, transcoded),
                    )
                } else {
                    let title = metadata
                        .info
                        .name_or_else(|| format!("torrent {real_id}"))
                        .into_owned();

                    let container_id = if transcoded {
                        TRANSCODED_FLAG | upnp_id
                    } else {
                        upnp_id
                    };
                    let parent_id = if transcoded { TRANSCODED_FLAG } else { 0 };

                    Some(ItemOrContainer::Container(Container {
                        id: container_id,
                        parent_id: Some(parent_id),
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
        client_ip: Option<IpAddr>,
    ) -> Vec<ItemOrContainer> {
        let mode = self.transcode_mode(client_ip);

        // Original root
        if object_id == 0 {
            match mode {
                TranscodeMode::OriginalOnly => {
                    let root = self.build_root(http_hostname, false);
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
                TranscodeMode::TranscodedOnly => {
                    // Root shows the transcoded view directly — no original files, no sub-folder.
                    let root = self.build_root(http_hostname, true);
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
                TranscodeMode::Both => {
                    let root = self.build_root(http_hostname, false);
                    if metadata {
                        return vec![ItemOrContainer::Container(Container {
                            id: 0,
                            parent_id: None,
                            children_count: Some(root.len() + 1), // +1 for Transcoded folder
                            title: "root".to_owned(),
                        })];
                    }
                    let mut result = root;
                    result.push(ItemOrContainer::Container(Container {
                        id: TRANSCODED_FLAG,
                        parent_id: Some(0),
                        title: "Transcoded".to_owned(),
                        children_count: None,
                    }));
                    return result;
                }
            }
        }

        // Transcoded root (only reachable in Both mode)
        if object_id == TRANSCODED_FLAG {
            let root = self.build_root(http_hostname, true);
            if metadata {
                return vec![ItemOrContainer::Container(Container {
                    id: TRANSCODED_FLAG,
                    parent_id: Some(0),
                    children_count: Some(root.len()),
                    title: "Transcoded".to_owned(),
                })];
            }
            return root;
        }

        let transcoded = (object_id & TRANSCODED_FLAG) != 0;
        let actual_id = object_id & !TRANSCODED_FLAG;

        let (node_id, torrent_id) = match decode_id(actual_id) {
            Ok((node_id, torrent_id)) => (node_id, torrent_id),
            Err(_) => {
                debug!(id=?object_id, "invalid id");
                return vec![];
            }
        };
        trace!(object_id, node_id, torrent_id, transcoded);

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
                transcoded,
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
                    transcoded,
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
        client_ip: Option<IpAddr>,
    ) -> Vec<ItemOrContainer> {
        self.build_impl(object_id, http_hostname, false, client_ip)
    }

    fn browse_metadata(
        &self,
        object_id: usize,
        http_hostname: &str,
        client_ip: Option<IpAddr>,
    ) -> Vec<ItemOrContainer> {
        self.build_impl(object_id, http_hostname, true, client_ip)
    }
}

impl Session {
    pub async fn make_upnp_adapter(
        self: &Arc<Self>,
        friendly_name: String,
        http_listen_port: u16,
    ) -> anyhow::Result<UpnpServer> {
        let renderer_capabilities: Arc<
            dashmap::DashMap<IpAddr, RendererCapabilities>,
        > = Arc::new(dashmap::DashMap::new());

        let adapter = UpnpServerSessionAdapter {
            session: self.clone(),
            renderer_capabilities: renderer_capabilities.clone(),
        };

        UpnpServer::new(UpnpServerOptions {
            friendly_name,
            http_listen_port,
            http_prefix: "/upnp".to_owned(),
            browse_provider: Box::new(adapter),
            cancellation_token: self.cancellation_token().child_token(),
            client_ip_extractor: Some(Arc::new(|ext| {
                ext.get::<ConnectInfo<WrappedSocketAddr>>()
                    .map(|ci| ci.0.ip())
            })),
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
            TRANSCODED_FLAG, TorrentFileTree, TorrentFileTreeNode, UpnpServerSessionAdapter,
            decode_id, encode_id,
        },
    };

    fn create_torrent(name: Option<&str>, files: &[&str]) -> TorrentMetaV1Owned {
        TorrentMetaV1Owned {
            announce: None,
            announce_list: vec![],
            info: bencode::WithRawBytes {
                data: TorrentMetaV1Info {
                    name: name.map(|n| n.as_bytes().into()),
                    pieces: b""[..].into(),
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

    fn make_adapter(session: Arc<Session>) -> UpnpServerSessionAdapter {
        UpnpServerSessionAdapter {
            session,
            renderer_capabilities: Arc::new(dashmap::DashMap::new()),
        }
    }

    use std::sync::Arc;

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

        let adapter = make_adapter(session);

        // Unknown client IP → Both mode: 2 torrents + Transcoded folder
        assert_eq!(
            adapter.browse_metadata(0, "127.0.0.1", None),
            vec![ItemOrContainer::Container(Container {
                id: 0,
                parent_id: None,
                children_count: Some(3),
                title: "root".into()
            })]
        );

        assert_eq!(
            adapter.browse_direct_children(0, "127.0.0.1", None),
            vec![
                ItemOrContainer::Item(Item {
                    id: encode_id(0, 0),
                    parent_id: 0,
                    title: "f1".into(),
                    mime_type: None,
                    url: "http://127.0.0.1/torrents/0/stream/0/f1".into(),
                    size: 1,
                    seekable: true,
                }),
                ItemOrContainer::Container(Container {
                    id: encode_id(0, 1),
                    parent_id: Some(0),
                    children_count: None,
                    title: "t2".into()
                }),
                ItemOrContainer::Container(Container {
                    id: TRANSCODED_FLAG,
                    parent_id: Some(0),
                    children_count: None,
                    title: "Transcoded".into()
                }),
            ]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(0, 0), "127.0.0.1", None),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(0, 0),
                parent_id: 0,
                title: "f1".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/0/stream/0/f1".into(),
                size: 1,
                seekable: true,
            })]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(0, 1), "127.0.0.1", None),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(0, 1),
                parent_id: Some(0),
                children_count: Some(1),
                title: "t2".into()
            })]
        );

        assert_eq!(
            adapter.browse_direct_children(encode_id(0, 1), "127.0.0.1", None),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(1, 1),
                parent_id: Some(encode_id(0, 1)),
                children_count: Some(1),
                title: "d1".into()
            }),]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(1, 1), "127.0.0.1", None),
            vec![ItemOrContainer::Container(Container {
                id: encode_id(1, 1),
                parent_id: Some(encode_id(0, 1)),
                children_count: Some(1),
                title: "d1".into()
            }),]
        );

        assert_eq!(
            adapter.browse_direct_children(encode_id(1, 1), "127.0.0.1", None),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(2, 1),
                parent_id: encode_id(1, 1),
                title: "f2".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/1/stream/0/d1/f2".into(),
                size: 1,
                seekable: true,
            })]
        );

        assert_eq!(
            adapter.browse_metadata(encode_id(2, 1), "127.0.0.1", None),
            vec![ItemOrContainer::Item(Item {
                id: encode_id(2, 1),
                parent_id: encode_id(1, 1),
                title: "f2".into(),
                mime_type: None,
                url: "http://127.0.0.1/torrents/1/stream/0/d1/f2".into(),
                size: 1,
                seekable: true,
            })]
        );

        // Transcoded root
        assert_eq!(
            adapter.browse_metadata(TRANSCODED_FLAG, "127.0.0.1", None),
            vec![ItemOrContainer::Container(Container {
                id: TRANSCODED_FLAG,
                parent_id: Some(0),
                children_count: Some(2),
                title: "Transcoded".into()
            })]
        );

        let transcoded_children =
            adapter.browse_direct_children(TRANSCODED_FLAG, "127.0.0.1", None);
        assert_eq!(transcoded_children.len(), 2);
        assert!(matches!(
            &transcoded_children[0],
            ItemOrContainer::Item(Item { id, url, seekable: false, .. })
            if *id == TRANSCODED_FLAG | encode_id(0, 0) && url.contains("/transcode/")
        ));
        assert!(matches!(
            &transcoded_children[1],
            ItemOrContainer::Container(Container { id, parent_id: Some(pid), .. })
            if *id == TRANSCODED_FLAG | encode_id(0, 1) && *pid == TRANSCODED_FLAG
        ));

        // DTS-capable client (OriginalOnly mode) → no Transcoded folder
        let dts_ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        adapter.renderer_capabilities.insert(
            dts_ip,
            upnp_serve::state::RendererCapabilities { supports_dts: true },
        );
        let root_for_dts = adapter.browse_direct_children(0, "127.0.0.1", Some(dts_ip));
        assert_eq!(root_for_dts.len(), 2); // 2 torrents, no Transcoded folder
        assert!(root_for_dts
            .iter()
            .all(|i| !matches!(i, ItemOrContainer::Container(c) if c.title == "Transcoded")));

        // Non-DTS client (TranscodedOnly mode) → root shows transcoded directly
        let nodts_ip: std::net::IpAddr = "10.0.0.2".parse().unwrap();
        adapter.renderer_capabilities.insert(
            nodts_ip,
            upnp_serve::state::RendererCapabilities { supports_dts: false },
        );
        let root_for_nodts = adapter.browse_direct_children(0, "127.0.0.1", Some(nodts_ip));
        assert_eq!(root_for_nodts.len(), 2); // 2 transcoded items, no Transcoded sub-folder
        assert!(matches!(
            &root_for_nodts[0],
            ItemOrContainer::Item(Item { url, seekable: false, .. })
            if url.contains("/transcode/")
        ));
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
