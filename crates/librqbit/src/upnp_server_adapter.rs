use std::{
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        HashMap,
    },
    sync::Arc,
};

use crate::{session::TorrentId, ManagedTorrent, Session};

#[derive(Clone)]
pub struct UpnpServerSessionAdapter {
    session: Arc<Session>,
    hostname: String,
    port: u16,
}

use anyhow::Context;
use buffers::ByteBufOwned;
use itertools::Itertools;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::{trace, warn};
use upnp_serve::{
    upnp_types::content_directory::{
        response::{Container, Item, ItemOrContainer},
        ContentDirectoryBrowseProvider,
    },
    UpnpServer, UpnpServerOptions,
};

struct TorrentFileTreeNode {
    title: String,
    parent_id: Option<usize>,
    children: Vec<usize>,

    real_torrent_file_id: Option<usize>,
}

impl TorrentFileTreeNode {
    fn as_item_or_container(
        &self,
        id: usize,
        torrent: &ManagedTorrent,
        adapter: &UpnpServerSessionAdapter,
    ) -> ItemOrContainer {
        match self.real_torrent_file_id {
            Some(f) => {
                return ItemOrContainer::Item(Item {
                    id,
                    parent_id: self.parent_id,
                    title: self.title.clone(),
                    mime_type: mime_guess::from_path(
                        &torrent.shared().file_infos[f].relative_filename,
                    )
                    .first(),
                    url: format!(
                        "http://{}:{}/torrents/{}/stream/0/{}",
                        adapter.hostname,
                        adapter.port,
                        torrent.id(),
                        self.title
                    ),
                })
            }
            None => ItemOrContainer::Container(Container {
                id,
                parent_id: self.parent_id,
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

impl TorrentFileTree {
    fn build(torent_id: TorrentId, info: &TorrentMetaV1Info<ByteBufOwned>) -> anyhow::Result<Self> {
        if info.iter_filenames_and_lengths()?.count() == 1 {
            let filename = info
                .iter_filenames_and_lengths()?
                .next()
                .context("bug")?
                .0
                .iter_components()
                .last()
                .context("bug")??;
            let root_node = TorrentFileTreeNode {
                title: filename.to_owned(),
                parent_id: None,
                children: vec![],
                real_torrent_file_id: Some(0),
            };
            return Ok(TorrentFileTree {
                nodes: vec![root_node],
            });
        }

        let root_node = TorrentFileTreeNode {
            title: match info.name.as_ref() {
                Some(n) => std::str::from_utf8(n)?.to_owned(),
                None => {
                    format!("torrent {}", torent_id)
                }
            },
            parent_id: None,
            children: vec![],
            real_torrent_file_id: None,
        };

        let mut tree = TorrentFileTree {
            nodes: vec![root_node],
        };

        let mut name_cache = HashMap::new();

        for (fid, (fi, _)) in info.iter_filenames_and_lengths()?.enumerate() {
            let components = match fi.to_vec() {
                Ok(v) => v,
                Err(_) => continue,
            };
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
                            parent_id: None,
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
    fn build_root(&self) -> Vec<ItemOrContainer> {
        let all = self
            .session
            .with_torrents(|torrents| torrents.map(|(_, t)| t.clone()).collect_vec());

        all.iter()
            .filter_map(|t| {
                let real_id = t.id();
                let upnp_id = real_id + 1;

                if t.shared().file_infos.len() == 1 {
                    // Just add the file directly
                    let rf = &t.shared().file_infos[0].relative_filename;
                    let title = rf.file_name()?.to_str()?.to_owned();
                    let mime_type = mime_guess::from_path(rf).first();
                    let url = format!(
                        "http://{}:{}/torrents/{real_id}/stream/0/{title}",
                        self.hostname, self.port
                    );
                    Some(ItemOrContainer::Item(Item {
                        id: upnp_id,
                        parent_id: Some(0),
                        title,
                        mime_type,
                        url,
                    }))
                } else {
                    let title = t
                        .shared()
                        .info
                        .name
                        .as_ref()
                        .and_then(|b| std::str::from_utf8(&b.0).ok())
                        .map(|n| n.to_owned())
                        .unwrap_or_else(|| format!("torrent {real_id}"));

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
}

impl ContentDirectoryBrowseProvider for UpnpServerSessionAdapter {
    fn browse_direct_children(&self, parent_id: usize) -> Vec<ItemOrContainer> {
        if parent_id == 0 {
            return self.build_root();
        }

        let torrent_id = {
            let torrent_id_plus_one = parent_id & 0xffffffff;
            if torrent_id_plus_one == 0 {
                return vec![];
            }
            torrent_id_plus_one - 1
        };

        let torrent = match self.session.get(torrent_id.into()) {
            Some(t) => t,
            None => {
                warn!(torrent_id, "no such torrent");
                return vec![];
            }
        };

        let tree = match TorrentFileTree::build(torrent.id(), &torrent.shared().info) {
            Ok(tree) => tree,
            Err(e) => {
                warn!(parent_id, error=?e, "error building torrent file tree");
                return vec![];
            }
        };

        let node_id = parent_id >> 4;
        trace!(node_id, parent_id);

        let node = match tree.nodes.get(node_id) {
            Some(n) => n,
            None => {
                warn!(torrent_id, node_id, "no such internal ID in torrent");
                return vec![];
            }
        };

        let mut result = Vec::new();

        if node.real_torrent_file_id.is_some() {
            result.push(node.as_item_or_container(node_id, &torrent, self))
        } else {
            for child_node in node.children.iter().filter_map(|id| tree.nodes.get(*id)) {
                result.push(child_node.as_item_or_container(node_id, &torrent, self));
            }
        };

        result
    }
}

impl Session {
    pub async fn make_upnp_adapter(
        self: &Arc<Self>,
        friendly_name: String,
        http_hostname: String,
        http_listen_port: u16,
    ) -> anyhow::Result<UpnpServer> {
        UpnpServer::new(UpnpServerOptions {
            friendly_name,
            http_hostname: http_hostname.clone(),
            http_listen_port,
            http_prefix: "/upnp".to_owned(),
            browse_provider: Box::new(UpnpServerSessionAdapter {
                session: self.clone(),
                hostname: http_hostname,
                port: http_listen_port,
            }),
        })
        .await
        .context("error creating upnp adapter")
    }
}
