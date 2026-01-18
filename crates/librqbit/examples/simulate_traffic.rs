use std::{
    io::Write,
    net::Ipv6Addr,
    num::NonZero,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ConnectionOptions, CreateTorrentOptions,
    CreateTorrentResult, ListenerOptions, PeerConnectionOptions, Session, SessionOptions,
    create_torrent, generate_azereus_style,
    http_api::{HttpApi, HttpApiOptions},
    limits::LimitsConfig,
    spawn_utils::BlockingSpawner,
    tracing_subscriber_config_utils::{InitLoggingOptions, init_logging},
};
use librqbit_core::constants::CHUNK_SIZE;
use librqbit_dualstack_sockets::{BindOpts, TcpListener};
use rand::{RngCore, SeedableRng, seq::IndexedRandom};
use tracing::info;

/// Base port for test sessions. Main uses 50000, peers use 50001+.
const BASE_PORT: u16 = 50000;

struct TestHarness {
    td: PathBuf,
    torrents: Vec<FakeTorrent>,
}

fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    info!(?path, "creating temp file");

    const BUF_SIZE: usize = 8192 * 16;
    let mut rng = rand::rngs::SmallRng::from_os_rng();
    let mut write_buf = [0; BUF_SIZE];
    while size > 0 {
        rng.fill_bytes(&mut write_buf[..]);
        let written = file.write(&write_buf[..size.min(BUF_SIZE)]).unwrap();
        size -= written;
    }
}

fn create_default_random_dir_with_torrents(dir: &Path, num_files: usize, file_size: usize) {
    for f in 0..num_files {
        create_new_file_with_random_content(&dir.join(format!("{f}.data")), file_size);
    }
}

fn generate_unique_torrent_name(index: usize) -> String {
    static TORRENT_NAMES: &[&str] = &[
        "Ubuntu 24.04 LTS Desktop amd64",
        "Arch.Linux.2026.01.01.x86_64.Rolling-RELEASE",
        "Debian.12.5.0.DVD.x64-iND",
        "Fedora-Workstation-41-x86_64 [ISO]",
        "Kali Linux 2025 4 Installer amd64",
        "Manjaro.KDE.Plasma.v24.0.Stable.x64.Multi-Language",
        "linuxmint-22-cinnamon-64bit-v2",
        "Tails amd64 v6.0.1 IMG",
        "NixOS_24.11_x86_64_Plasma_Gnome_Dual_Boot",
        "Pop_OS_22.04_Intel_Nvidia_Edition",
        "Gentoo.Admin.CD.20241229.x86",
        "AlmaLinux-9.4-x86_64-Boot-Full",
        "FreeBSD-14.1-STABLE-amd64-KMS",
        "openSUSE.Leap.15.6.DVD.x86_64.Build.0412-Scene",
        "Slackware 15 0 x64 DVD1",
        "Void.Live.x86_64.20240314.XFCE.PROPER-GRP",
        "EndeavourOS_Galileo_v2_Neo",
        "Rocky-9.3-x86_64-Minimal-Standard",
        "Zorin-OS-17.1-Core-64bit.iso",
        "Alpine.3.20.0.Virt",
        "TrueNAS.Scale.24.04.0.Dragonfish",
        "Clonezilla.Live.3.1.2.AMD64.NoArch",
        "Garuda-Dr460nized-240501-Gaming",
        "Puppy_Linux_9.5_x64",
        "SteamOS-Holoiso-v3.5.19-Deck-Experience",
        "Red.Hat.Enterprise.Linux.v9.4.x64.dvd-TiNYiSO",
        "CentOS-Stream-9-latest-x86_64-dvd1.iso",
        "ElementaryOS-7.1-Horus-20240520",
        "Parrot-Security-6.0-LTS-amd64",
        "Lubuntu.24.04.LTS.Minimal.Install.x64",
    ];

    let base_name = TORRENT_NAMES[index % TORRENT_NAMES.len()];
    let cycle = index / TORRENT_NAMES.len();

    if cycle == 0 {
        base_name.to_string()
    } else {
        format!("{} ({})", base_name, cycle + 1)
    }
}

async fn create_one(path: &Path, index: usize) -> anyhow::Result<CreateTorrentResult> {
    create_default_random_dir_with_torrents(
        path,
        rand::random_range(2..10),
        rand::random_range(1..8) * 1024 * 1024,
    );
    create_torrent(
        path,
        CreateTorrentOptions {
            name: Some(&generate_unique_torrent_name(index)),
            piece_length: Some(CHUNK_SIZE),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
}

struct FakeTorrent {
    root: PathBuf,
    torrent_file: Bytes,
}

async fn create_torrents(td: &Path, count: usize) -> anyhow::Result<Vec<FakeTorrent>> {
    let mut result = Vec::new();
    for torrent in 0..count {
        let dir = td.join(torrent.to_string());
        tokio::fs::create_dir_all(&dir).await?;
        let res = create_one(&dir, torrent).await?;
        result.push(FakeTorrent {
            root: dir,
            torrent_file: res.as_bytes()?,
        })
    }
    Ok(result)
}

impl TestHarness {
    async fn run_forever(self) -> anyhow::Result<()> {
        let _peers = self.start_peers().await?;
        self.run_main().await.context("error running main")
    }

    async fn start_peer(&self, id: usize) -> anyhow::Result<Arc<Session>> {
        let out = self.td.join(format!("peer_{}", id));
        let listen_mode = [
            librqbit::ListenerMode::TcpOnly,
            librqbit::ListenerMode::UtpOnly,
        ]
        .choose(&mut rand::rng())
        .copied()
        .unwrap();

        let listen_port = BASE_PORT + 1 + id as u16; // 50001, 50002, etc.
        let peer_id = generate_azereus_style(*b"rQ", librqbit_core::crate_version!());
        let root_span = tracing::info_span!("peer", id, port = listen_port);

        let session = Session::new_with_opts(
            out.clone(),
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                fastresume: false,
                persistence: None,
                peer_id: Some(peer_id),
                root_span: Some(root_span),
                listen: Some(ListenerOptions {
                    mode: listen_mode,
                    listen_addr: (Ipv6Addr::UNSPECIFIED, listen_port).into(),
                    enable_upnp_port_forwarding: false,
                    utp_opts: None,
                    announce_port: None,
                    ipv4_only: false,
                }),
                ratelimits: LimitsConfig {
                    upload_bps: NonZero::new(64 * 1024),
                    download_bps: Default::default(),
                },
                disable_local_service_discovery: false,
                connect: Some(ConnectionOptions {
                    proxy_url: None,
                    enable_tcp: listen_mode.tcp_enabled(),
                    peer_opts: Some(PeerConnectionOptions {
                        connect_timeout: Some(Duration::from_secs(1)),
                        read_write_timeout: Some(Duration::from_secs(32)),
                        keep_alive_interval: None,
                    }),
                }),
                ..Default::default()
            },
        )
        .await?;
        for (tid, torrent) in self.torrents.iter().enumerate() {
            let opts = if tid == id {
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(torrent.root.to_string_lossy().into_owned()),
                    ..Default::default()
                })
            } else {
                // Don't cross-seed too often
                if rand::random_bool(0.9) {
                    continue;
                }
                Some(AddTorrentOptions {
                    output_folder: Some(
                        out.join(format!("torrent_{}", tid))
                            .to_str()
                            .map(|s| s.to_owned())
                            .unwrap(),
                    ),
                    ..Default::default()
                })
            };
            let t = session
                .add_torrent(
                    AddTorrent::TorrentFileBytes(torrent.torrent_file.clone()),
                    opts,
                )
                .await
                .with_context(|| format!("error adding torrent {tid}"))?
                .into_handle()
                .context("into handle")?;
            if tid == id {
                t.wait_until_completed().await?;
            }
        }
        Ok(session)
    }

    async fn start_peers(&self) -> anyhow::Result<Vec<Arc<Session>>> {
        let mut peers = Vec::new();
        for id in 0..self.torrents.len() {
            peers.push(
                self.start_peer(id)
                    .await
                    .with_context(|| format!("error starting peer {id}"))?,
            );
        }

        Ok(peers)
    }

    async fn run_main(&self) -> anyhow::Result<()> {
        let path = self.td.join("main");

        let peer_id = generate_azereus_style(*b"rQ", librqbit_core::crate_version!());
        let root_span = tracing::info_span!("main", port = BASE_PORT);

        let session = Session::new_with_opts(
            path.clone(),
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                fastresume: false,
                persistence: None,
                peer_id: Some(peer_id),
                root_span: Some(root_span),
                listen: Some(ListenerOptions {
                    mode: librqbit::ListenerMode::TcpAndUtp,
                    listen_addr: (Ipv6Addr::UNSPECIFIED, BASE_PORT).into(),
                    enable_upnp_port_forwarding: false,
                    utp_opts: None,
                    announce_port: None,
                    ipv4_only: false,
                }),
                disable_local_service_discovery: false,
                connect: Some(ConnectionOptions {
                    proxy_url: None,
                    enable_tcp: true,
                    peer_opts: Some(PeerConnectionOptions {
                        connect_timeout: Some(Duration::from_secs(1)),
                        read_write_timeout: Some(Duration::from_secs(32)),
                        keep_alive_interval: None,
                    }),
                }),
                ..Default::default()
            },
        )
        .await?;
        for (id, torrent) in self.torrents.iter().enumerate() {
            session
                .add_torrent(
                    AddTorrent::from_bytes(torrent.torrent_file.clone()),
                    Some(AddTorrentOptions {
                        output_folder: Some(
                            path.join(format!("torrent_{id}"))
                                .to_str()
                                .unwrap()
                                .to_owned(),
                        ),
                        ..Default::default()
                    }),
                )
                .await
                .with_context(|| format!("error adding torrent {id}"))?;
        }

        let api = Api::new(session.clone(), None, None);
        let http = HttpApi::new(
            api,
            Some(HttpApiOptions {
                read_only: false,
                basic_auth: None,
                allow_create: true,
                prometheus_handle: None,
            }),
        );
        let sock = TcpListener::bind_tcp(
            (Ipv6Addr::UNSPECIFIED, 3030).into(),
            BindOpts {
                request_dualstack: true,
                reuseport: true,
                device: None,
            },
        )?;

        http.make_http_api_and_run(sock, None).await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join("rqbit-simulate-traffic");

    // Clean up before creating log file
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;

    let log_file = std::env::var("TESTSERVER_LOG_FILE")
        .unwrap_or_else(|_| root.join("testserver.log").to_string_lossy().into_owned());
    let log_file_rust_log = std::env::var("TESTSERVER_LOG_FILE_RUST_LOG").ok();

    let _logging = init_logging(InitLoggingOptions {
        default_rust_log_value: Some("info"),
        log_file: Some(&log_file),
        log_file_rust_log: Some(log_file_rust_log.as_deref().unwrap_or("debug")),
    })?;

    info!("logging to file: {}", log_file);

    TestHarness {
        torrents: create_torrents(&root.join("torrents"), 10).await?,
        td: root,
    }
    .run_forever()
    .await?;
    Ok(())
}
