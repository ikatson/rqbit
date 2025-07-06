use librqbit_upnp_serve::{
    UpnpServerOptions, services::content_directory::ContentDirectoryBrowseProvider,
};

struct Dummy;

impl ContentDirectoryBrowseProvider for Dummy {
    fn browse_direct_children(
        &self,
        parent_id: usize,
        http_hostname: &str,
    ) -> Vec<librqbit_upnp_serve::services::content_directory::browse::response::ItemOrContainer>
    {
        todo!()
    }

    fn browse_metadata(
        &self,
        object_id: usize,
        http_hostname: &str,
    ) -> Vec<librqbit_upnp_serve::services::content_directory::browse::response::ItemOrContainer>
    {
        todo!()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    librqbit_upnp_serve::UpnpServer::new(UpnpServerOptions {
        friendly_name: "test".into(),
        http_listen_port: 3030,
        http_prefix: "/".into(),
        browse_provider: std::hint::black_box(Box::new(Dummy)),
        cancellation_token: Default::default(),
    })
    .await
    .unwrap();
}
