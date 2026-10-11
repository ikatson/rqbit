use std::{net::Ipv4Addr, time::Duration};

use anyhow::{Context, bail};
use bytes::Bytes;
use librqbit_core::torrent_metainfo::TorrentMetaV1Borrowed;
use sha1w::{ISha1, Sha1};
use tokio::time::timeout;
use tracing::{error_span, info};

use crate::{
    AddTorrentOptions, ConnectionOptions, ListenerMode, ListenerOptions, Session, SessionOptions,
    tests::test_util::setup_test_logging,
};

// A BEP 52 hybrid torrent's v1 compat layer must use BEP 47 padding files to
// align files to piece boundaries, and BEP 47's recommended pad name is
// [".pad", "<length>"], so two equal-length pads share one path. This e2e
// downloads such a torrent (two identical ".pad/24" entries) between two real
// sessions: metadata validation must accept the duplicate pad paths, piece
// hashing must treat pads as zeros on both sides, and no pad must ever be
// materialized on disk.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_download_duplicate_pad_filenames() -> anyhow::Result<()> {
    timeout(
        Duration::from_secs(60),
        e2e_download_duplicate_pad_filenames(),
    )
    .await
    .context("e2e timed out")?
}

async fn e2e_download_duplicate_pad_filenames() -> anyhow::Result<()> {
    setup_test_logging();

    const PIECE_LENGTH: usize = 1024;
    const FILE_LENGTH: usize = 1000;
    const PAD_LENGTH: usize = 24;
    let seeder_dir = tempfile::TempDir::with_prefix("rqbit_e2e_pads_seed").unwrap();
    let a_contents = vec![0xau8; FILE_LENGTH];
    let b_contents = vec![0xbu8; FILE_LENGTH];
    std::fs::write(seeder_dir.path().join("a.bin"), &a_contents).unwrap();
    std::fs::write(seeder_dir.path().join("b.bin"), &b_contents).unwrap();

    // Piece space is [a.bin][.pad/24][b.bin][.pad/24]: each piece is one file
    // plus one all-zero pad.
    let mut pieces = Vec::new();
    for chunk in [&a_contents, &b_contents] {
        let mut h = Sha1::new();
        h.update(chunk);
        h.update(&[0u8; PAD_LENGTH]);
        pieces.extend_from_slice(&h.finish());
    }
    let torrent_bytes = build_padded_torrent(&pieces, PIECE_LENGTH, FILE_LENGTH, PAD_LENGTH);
    {
        let t: TorrentMetaV1Borrowed =
            librqbit_core::torrent_metainfo::torrent_from_bytes(&torrent_bytes).unwrap();
        t.info.data.clone().validate().unwrap();
    }

    // 1. Seed from the directory with only the real files on disk.
    let seeder = Session::new_with_opts(
        seeder_dir.path().to_owned(),
        SessionOptions {
            dht: None,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: (Ipv4Addr::LOCALHOST, 15177).into(),
                ..Default::default()
            }),
            disable_local_service_discovery: true,
            root_span: Some(error_span!(parent: None, "seeder")),
            ..Default::default()
        },
    )
    .await
    .context("error starting seeder session")?;

    let seeder_handle = seeder
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Bytes::from(torrent_bytes.clone())),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(seeder_dir.path().to_str().unwrap().to_owned()),
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent to seeder")?
        .into_handle()
        .context("into_handle()")?;

    timeout(Duration::from_secs(30), async {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            let is_live = seeder_handle
                .with_state(|s| match s {
                    crate::ManagedTorrentState::Initializing(_) => Ok(false),
                    crate::ManagedTorrentState::Live(l) => {
                        if !l.is_finished() {
                            bail!("seeder went live but is not finished")
                        }
                        Ok(true)
                    }
                    crate::ManagedTorrentState::Error(e) => bail!("seeder error: {e:#}"),
                    _ => bail!("broken state"),
                })
                .context("error checking for torrent liveness")?;
            if is_live {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("timeout waiting for seeder to become live")??;

    let seeder_addr = seeder
        .listen_addr()
        .context("expected listen_addr to be set")?;
    info!(%seeder_addr, "seeder is live");

    // 2. Download into an empty folder.
    let dl_root = tempfile::TempDir::with_prefix("rqbit_e2e_pads_dl").unwrap();
    let dl_dir = dl_root.path().join("out");
    let client = Session::new_with_opts(
        dl_dir.clone(),
        SessionOptions {
            dht: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            disable_local_service_discovery: true,
            root_span: Some(error_span!(parent: None, "client")),
            ..Default::default()
        },
    )
    .await
    .context("error starting client session")?;

    let client_handle = client
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Bytes::from(torrent_bytes)),
            Some(AddTorrentOptions {
                initial_peers: Some(vec![seeder_addr]),
                output_folder: Some(dl_dir.to_str().unwrap().to_owned()),
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent to client")?
        .into_handle()
        .context("into_handle()")?;

    timeout(
        Duration::from_secs(45),
        client_handle.wait_until_completed(),
    )
    .await
    .context("timeout waiting for download")?
    .context("download error")?;
    info!("client download completed");

    // 3. The real files match, and no padding file was ever written.
    assert_eq!(
        std::fs::read(dl_dir.join("a.bin"))?,
        a_contents,
        "a.bin content mismatch"
    );
    assert_eq!(
        std::fs::read(dl_dir.join("b.bin"))?,
        b_contents,
        "b.bin content mismatch"
    );
    let entries: Vec<_> = std::fs::read_dir(&dl_dir)
        .context("read_dir dl_dir")?
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "expected only a.bin and b.bin on disk, got {entries:?}"
    );
    assert!(
        !dl_dir.join(".pad").exists(),
        "padding files must not be materialized on disk"
    );

    drop(client_handle);
    drop(seeder_handle);
    drop(client);
    drop(seeder);
    info!("e2e finished");
    Ok(())
}

fn bstr(b: &[u8]) -> Vec<u8> {
    let mut o = format!("{}:", b.len()).into_bytes();
    o.extend_from_slice(b);
    o
}

fn bint(v: i64) -> Vec<u8> {
    format!("i{v}e").into_bytes()
}

fn bdict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
    let mut o = vec![b'd'];
    for (k, v) in entries {
        o.extend_from_slice(&bstr(k));
        o.extend_from_slice(v);
    }
    o.push(b'e');
    o
}

// A v1 multi-file metainfo shaped like the v1 compat layer of a BEP 52 hybrid:
// files are piece-aligned via BEP 47 padding entries named ".pad/<length>", and
// the two equal-length pads (both ".pad/24") collide by design.
fn build_padded_torrent(
    pieces: &[u8],
    piece_length: usize,
    file_length: usize,
    pad_length: usize,
) -> Vec<u8> {
    let push_file = |flist: &mut Vec<u8>, path: &[&[u8]], len: i64, attr: Option<&[u8]>| {
        let mut path_enc = vec![b'l'];
        for bit in path {
            path_enc.extend_from_slice(&bstr(bit));
        }
        path_enc.push(b'e');
        let mut entries: Vec<(&[u8], Vec<u8>)> = vec![
            (b"length".as_slice(), bint(len)),
            (b"path".as_slice(), path_enc),
        ];
        if let Some(a) = attr {
            entries.push((b"attr".as_slice(), bstr(a)));
        }
        flist.extend_from_slice(&bdict(&entries));
    };

    let pad_name = pad_length.to_string();
    let mut flist = vec![b'l'];
    push_file(&mut flist, &[b"a.bin"], file_length as i64, None);
    push_file(
        &mut flist,
        &[b".pad", pad_name.as_bytes()],
        pad_length as i64,
        Some(b"p"),
    );
    push_file(&mut flist, &[b"b.bin"], file_length as i64, None);
    push_file(
        &mut flist,
        &[b".pad", pad_name.as_bytes()],
        pad_length as i64,
        Some(b"p"),
    );
    flist.push(b'e');

    let info = bdict(&[
        (b"files".as_slice(), flist),
        (b"name".as_slice(), bstr(b"padded-hybrid")),
        (b"piece length".as_slice(), bint(piece_length as i64)),
        (b"pieces".as_slice(), bstr(pieces)),
    ]);
    let mut top = vec![b'd'];
    top.extend_from_slice(&bstr(b"announce"));
    top.extend_from_slice(&bstr(b"http://127.0.0.1:0/announce"));
    top.extend_from_slice(&bstr(b"info"));
    top.extend_from_slice(&info);
    top.push(b'e');
    top
}
