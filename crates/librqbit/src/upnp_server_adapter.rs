use std::sync::Arc;

use crate::Session;

#[derive(Clone)]
pub struct UpnpServerSessionAdapter {
    session: Arc<Session>,
    hostname: String,
    port: u16,
}

use anyhow::Context;
use tracing::warn;
use upnp_serve::{
    upnp_types::content_directory::{
        response::{Item, ItemOrContainer},
        ContentDirectoryBrowseProvider,
    },
    UpnpServer, UpnpServerOptions,
};

impl ContentDirectoryBrowseProvider for UpnpServerSessionAdapter {
    fn browse_direct_children(&self, parent_id: usize) -> Vec<ItemOrContainer> {
        if parent_id != 0 {
            warn!(parent_id, "UPNP request for parent_id != 0, not supported");
            return vec![];
        }

        let hostname = &self.hostname;
        let port = self.port;
        let mut next_id = 0;
        self.session.with_torrents(|torrents| {
            torrents
                .flat_map(|(id, t)| {
                    t.shared()
                        .file_infos
                        .iter()
                        .enumerate()
                        .filter_map(move |(fid, fi)| {
                            let mime_type = mime_guess::from_path(&fi.relative_filename).first();
                            let title = fi.relative_filename.file_stem()?.to_string_lossy();
                            let url = format!(
                                "http://{hostname}:{port}/torrents/{id}/stream/{fid}/{title}"
                            );
                            let fake_id = next_id;
                            next_id += 1;
                            Some(ItemOrContainer::Item(Item {
                                title: title.into_owned(),
                                mime_type,
                                url,
                                id: fake_id,
                                parent_id: None,
                            }))
                        })
                })
                .collect()
        })
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
