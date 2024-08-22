use std::{io::Write, time::Duration};

use anyhow::Context;
use gethostname::gethostname;
use http_handlers::make_router;
use librqbit_sha1_wrapper::ISha1;
use ssdp::SsdpRunner;
use state::{ContentDirectoryBrowseProvider, UnpnServerState};

mod constants;
mod http_handlers;
mod ssdp;
mod state;
mod templates;
mod upnp;

pub struct UpnpServerOptions {
    pub friendly_name: String,
    pub http_hostname: String,
    pub http_listen_port: u16,
    pub http_prefix: String,
    pub browse_provider: Box<dyn ContentDirectoryBrowseProvider>,
}

pub struct UpnpServer {
    axum_router: Option<axum::Router<UnpnServerState>>,
    ssdp_runner: SsdpRunner,
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
    pub async fn new(opts: UpnpServerOptions) -> anyhow::Result<Self> {
        let usn = create_usn(&opts)?;

        let description_http_location = {
            let hostname = &opts.http_hostname;
            let port = opts.http_listen_port;
            let http_prefix = &opts.http_prefix;
            format!("http://{hostname}:{port}/{http_prefix}description.xml")
        };

        let ssdp_runner = crate::ssdp::SsdpRunner::new(ssdp::SsdpRunnerOptions {
            usn: usn.clone(),
            description_http_location,
            server_string: "Linux/3.4 UPnP/1.0 rqbit/1".to_owned(),
            notify_interval: Duration::from_secs(60),
        })
        .await
        .context("error initializing SsdpRunner")?;

        let router = make_router(
            opts.friendly_name,
            opts.http_prefix,
            usn,
            "librqbit-upnp-server 1.0".to_owned(),
            opts.http_listen_port,
            opts.browse_provider,
        )?;

        Ok(Self {
            axum_router: Some(router),
            ssdp_runner,
        })
    }

    pub fn take_router(&mut self) -> anyhow::Result<axum::Router<UnpnServerState>> {
        self.axum_router
            .take()
            .context("programming error: router already taken")
    }

    pub async fn run_ssdp_forever(self) -> anyhow::Result<()> {
        self.ssdp_runner
            .run_forever()
            .await
            .context("error running SSDP loop")
    }
}
