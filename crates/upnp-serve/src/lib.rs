use std::io::Write;

use anyhow::Context;
use gethostname::gethostname;
use http_handlers::make_router;
use librqbit_sha1_wrapper::ISha1;
use state::UnpnServerState;

mod constants;
mod http_handlers;
mod ssdp;
mod state;
mod templates;
mod upnp;

pub struct UpnpServerOptions {
    pub friendly_name: String,
    pub http_listen_port: u16,
    pub http_prefix: String,
}

pub struct UpnpServer {
    pub axum_router: axum::Router<UnpnServerState>,
}

fn create_usn(opts: &UpnpServerOptions) -> anyhow::Result<String> {
    let mut buf = Vec::new();

    buf.write_all(gethostname().as_encoded_bytes())?;
    write!(
        &mut buf,
        "{}{}{}",
        opts.friendly_name, opts.http_listen_port, opts.http_prefix
    )?;

    let mut sha1 = librqbit_sha1_wrapper::Sha1::new();
    sha1.update(&buf);

    let hash = sha1.finish();
    let uuid = uuid::Builder::from_slice(&hash)?.into_uuid();
    Ok(format!("uuid:{}", uuid))
}

impl UpnpServer {
    pub fn new(opts: UpnpServerOptions) -> anyhow::Result<Self> {
        let usn = create_usn(&opts)?;

        let router = make_router(
            opts.friendly_name,
            opts.http_prefix,
            usn,
            "librqbit-upnp-server 1.0".to_owned(),
            opts.http_listen_port,
        )?;

        Ok(Self {
            axum_router: router,
        })
    }
}
