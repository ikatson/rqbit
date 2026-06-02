pub mod commands;

use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::codec::{LengthDelimitedCodec, FramedRead};
use tokio::io::AsyncWriteExt;
use futures_util::stream::StreamExt;
use std::time::Duration;

use crate::Session;
use self::commands::WorkerCommand;

pub struct SocketRpcServer {
    session: Arc<Session>,
    auth_key: [u8; 32],
}

impl SocketRpcServer {
    pub fn new(session: Arc<Session>, auth_key: [u8; 32]) -> Self {
        Self { session, auth_key }
    }

    pub async fn listen(self, addr: &str) -> anyhow::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        let server = Arc::new(self);

        loop {
            let (mut stream, _addr) = listener.accept().await?;
            let server_clone = server.clone();

            tokio::spawn(async move {
                // Enable TCP Keep-Alive
                {
                    let sock_ref = socket2::SockRef::from(&stream);
                    let tcp_keepalive = socket2::TcpKeepalive::new()
                        .with_time(Duration::from_secs(60))
                        .with_interval(Duration::from_secs(15));
                    let _ = sock_ref.set_tcp_keepalive(&tcp_keepalive);
                }

                // Auth check
                let mut auth_buf = [0u8; 32];
                use tokio::io::AsyncReadExt;
                if let Ok(n) = stream.read_exact(&mut auth_buf).await {
                    if n != 32 || auth_buf != server_clone.auth_key {
                        return; // Drop connection
                    }
                } else {
                    return;
                }

                let (reader, mut writer) = stream.into_split();
                let mut framed = FramedRead::new(reader, LengthDelimitedCodec::new());

                while let Some(Ok(frame)) = framed.next().await {
                    let frame_bytes = frame.freeze();
                    match WorkerCommand::parse(&frame_bytes) {
                        Ok(cmd) => {
                            match cmd {
                                WorkerCommand::AssignTorrent(data) => {
                                    if let Ok(s) = std::str::from_utf8(data) {
                                        let session = server_clone.session.clone();
                                        let url = s.to_owned();
                                        tokio::spawn(async move {
                                            let _ = session.add_torrent(crate::AddTorrent::from_url(url), None).await;
                                        });
                                    }
                                }
                                WorkerCommand::Pause(info_hash) => {
                                    if let Some(handle) = server_clone.session.get(crate::api::TorrentIdOrHash::Hash(info_hash)) {
                                        let _ = handle.pause();
                                    }
                                }
                                WorkerCommand::RequestTelemetry => {
                                    let down_speed = server_clone.session.stats.down_speed_estimator.bps();
                                    let up_speed = server_clone.session.stats.up_speed_estimator.bps();

                                    let mut resp = Vec::new();
                                    resp.extend_from_slice(&down_speed.to_be_bytes());
                                    resp.extend_from_slice(&up_speed.to_be_bytes());

                                    let chunks_data = server_clone.session.with_torrents(|torrents| {
                                        let mut chunks_data = Vec::new();
                                        for (_, mt) in torrents {
                                            if let Ok(bits) = mt.with_chunk_tracker(|ct| ct.get_have_pieces().as_bytes().to_vec()) {
                                                chunks_data.extend_from_slice(&mt.info_hash().0);
                                                chunks_data.extend_from_slice(&(bits.len() as u32).to_be_bytes());
                                                chunks_data.extend_from_slice(&bits);
                                            }
                                        }
                                        chunks_data
                                    });

                                    resp.extend_from_slice(&(chunks_data.len() as u32).to_be_bytes());
                                    resp.extend_from_slice(&chunks_data);
                                    let _ = writer.write_all(&resp).await;
                                }
                            }
                        }
                        Err(_) => {
                            return; // Protocol violation, drop connection
                        }
                    }
                }
            });
        }
    }
}
