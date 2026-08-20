//! Message Stream Encryption (MSE) support.
//!
//! MSE obfuscates the BitTorrent handshake and stream with RC4 and a DH-768
//! key exchange (Azureus protocol encryption). Some peers (Xunlei, BitComet,
//! etc.) refuse plaintext handshakes, so MSE is required to talk to them.
//!
//! This module is built top-down: the [`MseMode`] config and the peer
//! connection wiring landed first, then the incoming (responder) handshake,
//! and the outgoing (initiator) handshake in a follow-up commit.

pub mod dh768;
pub mod rc4;
pub mod stream;

use anyhow::{Context, Result, bail};
use rand::{Rng, RngExt};
use sha1w::{ISha1, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};

use dh768::Dh768;
use rc4::Rc4;
use stream::{Rc4Reader, Rc4Writer};

const BT_PROTOCOL_PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";
const BT_HANDSHAKE_LEN: usize = 68;
const MAX_PAD: usize = 512;
const VC_LEN: usize = 8;
const CRYPTO_RC4: u32 = 2;

/// How MSE is applied to peer connections.
///
/// `Disabled` (the default) skips MSE entirely (plaintext only). `Enabled`
/// prefers MSE and falls back to a plaintext redial on failure. `Forced`
/// requires MSE: any peer that does not complete the MSE handshake is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MseMode {
    Disabled,
    Enabled,
    Forced,
}

// Manual impl rather than `#[derive(Default)]` + `#[default]`: keeping the
// default explicit here avoids silently carrying a `#[default]` annotation
// over if the default is ever flipped to another variant.
#[allow(clippy::derivable_impls)]
impl Default for MseMode {
    fn default() -> Self {
        Self::Disabled
    }
}

pub struct PrefixReader<R> {
    prefix: Vec<u8>,
    position: usize,
    inner: R,
}

impl<R> PrefixReader<R> {
    fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self {
            prefix,
            position: 0,
            inner,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for PrefixReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() && buf.remaining() != 0 {
            let count = (this.prefix.len() - this.position).min(buf.remaining());
            buf.put_slice(&this.prefix[this.position..this.position + count]);
            this.position += count;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

pub enum IncomingOutcome<R, W> {
    Encrypted {
        read: Rc4Reader<R>,
        write: Rc4Writer<W>,
        handshake_bytes: Vec<u8>,
        info_hash: [u8; 20],
    },
    Plaintext {
        read: PrefixReader<R>,
        write: W,
    },
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hash = Sha1::new();
    for part in parts {
        hash.update(part);
    }
    hash.finish()
}

fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut result = [0u8; 20];
    for i in 0..20 {
        result[i] = a[i] ^ b[i];
    }
    result
}

fn derive_keys(secret: &[u8], skey: &[u8], outgoing: bool) -> (Rc4, Rc4) {
    let (encrypt_key, decrypt_key) = if outgoing {
        (
            sha1(&[b"keyA", secret, skey]),
            sha1(&[b"keyB", secret, skey]),
        )
    } else {
        (
            sha1(&[b"keyB", secret, skey]),
            sha1(&[b"keyA", secret, skey]),
        )
    };
    let mut encrypt = Rc4::new(&encrypt_key);
    let mut decrypt = Rc4::new(&decrypt_key);
    encrypt.discard(1024);
    decrypt.discard(1024);
    (encrypt, decrypt)
}

async fn read_scan_for_needle<R: AsyncRead + Unpin>(
    read: &mut R,
    needle: &[u8],
    max_pad: usize,
) -> Result<usize> {
    let mut window = Vec::with_capacity(max_pad + needle.len());
    let mut byte = [0u8; 1];
    loop {
        read.read_exact(&mut byte)
            .await
            .context("disconnected while scanning MSE handshake")?;
        window.push(byte[0]);
        if window.ends_with(needle) {
            return Ok(window.len() - needle.len());
        }
        if window.len() >= max_pad + needle.len() {
            bail!("MSE pattern not found within {max_pad} pad bytes");
        }
    }
}

async fn read_encrypted<R: AsyncRead + Unpin>(
    read: &mut R,
    decrypt: &mut Rc4,
    bytes: &mut [u8],
) -> Result<()> {
    read.read_exact(bytes).await?;
    decrypt.apply_keystream(bytes);
    Ok(())
}

fn random_pad(max: usize) -> Vec<u8> {
    let length = rand::rng().random_range(0..=max);
    let mut pad = vec![0u8; length];
    rand::rng().fill_bytes(&mut pad);
    pad
}

/// Initiate MSE on a connected stream.
///
/// Not yet implemented: this placeholder is wired into the outgoing peer
/// connection path and will be replaced by the full handshake.
pub async fn outgoing<R, W>(
    read: R,
    write: W,
    info_hash: &[u8; 20],
    initial_payload: &[u8; 68],
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let _ = (read, write, info_hash, initial_payload);
    anyhow::bail!("MSE outgoing handshake not implemented")
}

/// Accept either a complete plaintext BitTorrent handshake or MSE. A
/// nonmatching partial plaintext prefix is retained as the beginning of YA.
pub async fn incoming<R, W, F>(
    mut read: R,
    mut write: W,
    lookup: F,
) -> Result<IncomingOutcome<R, W>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(&[u8; 20]) -> Option<[u8; 20]>,
{
    let mut prefix = Vec::with_capacity(BT_PROTOCOL_PREFIX.len());
    while prefix.len() < BT_PROTOCOL_PREFIX.len() {
        let mut byte = [0u8; 1];
        read.read_exact(&mut byte).await?;
        prefix.push(byte[0]);
        if prefix != BT_PROTOCOL_PREFIX[..prefix.len()] {
            break;
        }
    }
    if prefix.len() == BT_PROTOCOL_PREFIX.len() {
        debug!("peer sent plaintext BT handshake, using plaintext path");
        return Ok(IncomingOutcome::Plaintext {
            read: PrefixReader::new(prefix, read),
            write,
        });
    }

    debug!("peer did not send plaintext prefix, attempting MSE handshake");

    let mut client_public = [0u8; 96];
    client_public[..prefix.len()].copy_from_slice(&prefix);
    read.read_exact(&mut client_public[prefix.len()..]).await?;

    let dh = Dh768::generate(&mut rand::rng());
    let secret = dh
        .shared_secret(&client_public)
        .ok_or_else(|| anyhow::anyhow!("MSE degenerate remote DH key"))?;

    // The responder sends YB + PadB immediately, before waiting for req1.
    write.write_all(&dh.public_key_bytes()).await?;
    write.write_all(&random_pad(MAX_PAD)).await?;

    let req1 = sha1(&[b"req1", &secret]);
    read_scan_for_needle(&mut read, &req1, MAX_PAD).await?;
    let mut obfuscated_skey = [0u8; 20];
    read.read_exact(&mut obfuscated_skey).await?;
    let skey_hash = xor20(&obfuscated_skey, &sha1(&[b"req3", &secret]));
    let info_hash =
        lookup(&skey_hash).ok_or_else(|| anyhow::anyhow!("MSE unknown info hash in SKEY"))?;
    let (mut encrypt, mut decrypt) = derive_keys(&secret, &info_hash, false);

    let mut vc = [0u8; VC_LEN];
    read_encrypted(&mut read, &mut decrypt, &mut vc).await?;
    if vc != [0u8; VC_LEN] {
        warn!("MSE invalid verification constant, aborting handshake");
        bail!("MSE invalid verification constant");
    }
    let mut provide = [0u8; 4];
    read_encrypted(&mut read, &mut decrypt, &mut provide).await?;
    if u32::from_be_bytes(provide) & CRYPTO_RC4 == 0 {
        bail!("MSE peer does not offer RC4");
    }
    let mut pad_length = [0u8; 2];
    read_encrypted(&mut read, &mut decrypt, &mut pad_length).await?;
    let pad_length = u16::from_be_bytes(pad_length) as usize;
    if pad_length > MAX_PAD {
        bail!("MSE PadC exceeds {MAX_PAD} bytes");
    }
    let mut pad_c = vec![0u8; pad_length];
    read_encrypted(&mut read, &mut decrypt, &mut pad_c).await?;

    let mut ia_length = [0u8; 2];
    read_encrypted(&mut read, &mut decrypt, &mut ia_length).await?;
    let ia_length = u16::from_be_bytes(ia_length) as usize;
    if ia_length > BT_HANDSHAKE_LEN {
        bail!("MSE IA length exceeds {BT_HANDSHAKE_LEN} bytes");
    }
    let mut handshake_bytes = vec![0u8; ia_length];
    read_encrypted(&mut read, &mut decrypt, &mut handshake_bytes).await?;

    // Respond before waiting for the rest of the BT handshake. An initiator
    // with IA=0 may not send that data until PE4 selects the cipher.
    let pad_d = random_pad(MAX_PAD);
    let mut response = Vec::with_capacity(VC_LEN + 4 + 2 + pad_d.len());
    response.extend_from_slice(&[0u8; VC_LEN]);
    response.extend_from_slice(&CRYPTO_RC4.to_be_bytes());
    response.extend_from_slice(&(pad_d.len() as u16).to_be_bytes());
    response.extend_from_slice(&pad_d);
    encrypt.apply_keystream(&mut response);
    write.write_all(&response).await?;

    let mut remaining = vec![0u8; BT_HANDSHAKE_LEN - ia_length];
    read_encrypted(&mut read, &mut decrypt, &mut remaining).await?;
    handshake_bytes.extend_from_slice(&remaining);

    trace!("MSE incoming handshake complete, RC4 established");
    Ok(IncomingOutcome::Encrypted {
        read: Rc4Reader::new(read, decrypt),
        write: Rc4Writer::new(write, encrypt),
        handshake_bytes,
        info_hash,
    })
}

#[cfg(test)]
mod tests;
