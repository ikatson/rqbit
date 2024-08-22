use std::sync::Arc;

use crate::Session;

#[derive(Clone)]
pub struct UpnpServerSessionAdapter {
    session: Arc<Session>,
    hostname: String,
    port: u16,
}

use anyhow::Context;
use upnp_serve::{
    upnp_types::content_directory::{response::ItemOrContainer, ContentDirectoryBrowseProvider},
    UpnpServer, UpnpServerOptions,
};

impl ContentDirectoryBrowseProvider for UpnpServerSessionAdapter {
    fn browse_direct_children(&self, parent_id: usize) -> Vec<ItemOrContainer> {
        todo!()
    }

    // fn browse(&self) -> Vec<upnp_serve::ContentDirectoryBrowseItem> {
    //     let hostname = &self.hostname;
    //     let port = self.port;
    //     self.session.with_torrents(|torrents| {
    //         torrents
    //             .flat_map(|(id, t)| {
    //                 t.shared()
    //                     .file_infos
    //                     .iter()
    //                     .enumerate()
    //                     .map(move |(fid, fi)| {
    //                         let mime_type = mime_guess::from_path(&fi.relative_filename)
    //                             .first()
    //                             .map(|m| m.to_string());
    //                         let title = fi.relative_filename.to_string_lossy();
    //                         let url = format!(
    //                             "http://{hostname}:{port}/torrents/{id}/stream/{fid}/{title}"
    //                         );
    //                         ContentDirectoryBrowseItem {
    //                             title: title.into_owned(),
    //                             mime_type,
    //                             url,
    //                         }
    //                     })
    //             })
    //             .collect()
    //     })
    // }
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
