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
pub mod stream;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use rand::{Rng, RngExt};
use sha1w::{ISha1, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};

use ::rc4::{KeyInit, Rc4, StreamCipher};
use dh768::Dh768;
use stream::{Rc4Reader, Rc4Writer};

const BT_PROTOCOL_PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";
const BT_HANDSHAKE_LEN: usize = 68;
const MAX_PAD: usize = 512;
const VC_LEN: usize = 8;
const CRYPTO_RC4: u32 = 2;

/// How long to wait (after sending Ya + PadA) for a peer to send its first 20
/// bytes before treating it as a silent MSE responder. A plaintext peer sends
/// its 68-byte BT handshake immediately, so we detect it here instead of
/// waiting for the full read/write timeout.
const PLAINTEXT_SNIFF_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Encrypted carries the RC4 state and the full handshake; Plaintext is the
/// fast path. The size gap is inherent to holding cipher state, not a defect.
#[allow(clippy::large_enum_variant)]
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

/// Encrypted carries the RC4-wrapped streams; PlaintextPeer carries none.
#[allow(clippy::large_enum_variant)]
pub enum OutgoingOutcome<R, W> {
    /// MSE handshake completed; use the RC4-wrapped streams.
    Encrypted(Rc4Reader<R>, Rc4Writer<W>),
    /// The peer answered with a plaintext BitTorrent handshake within the
    /// sniff window. Abort MSE and redial plaintext on a fresh connection.
    PlaintextPeer,
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

fn derive_keys(secret: &[u8], skey: &[u8], outgoing: bool) -> (Rc4, Rc4, [u8; 20]) {
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
    let mut encrypt = Rc4::new_from_slice(&encrypt_key).expect("20-byte RC4 key");
    let mut decrypt = Rc4::new_from_slice(&decrypt_key).expect("20-byte RC4 key");
    // MSE drop-1024: discard the first 1024 keystream bytes.
    let mut drop = [0u8; 1024];
    encrypt.apply_keystream(&mut drop);
    decrypt.apply_keystream(&mut drop);
    (encrypt, decrypt, decrypt_key)
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

/// Probe the responder's first 20 bytes within a short window to detect a
/// plaintext peer (which sends its BT handshake immediately on connect).
///
/// Returns `Ok(Some(prefix))` with the 20 bytes read when they start with the
/// BT protocol prefix (peer is plaintext). Returns `Ok(None)` with the bytes
/// read so far when they do not (they are the leading bytes of the MSE
/// responder's DH public key), or when the window elapsed without a full 20
/// bytes (treat as a slow MSE responder). The partial `sniffed` bytes must be
/// preserved as the prefix of the DH public key.
async fn sniff_plaintext<R: AsyncRead + Unpin>(read: &mut R) -> Result<(Option<Vec<u8>>, Vec<u8>)> {
    let mut sniffed = Vec::with_capacity(BT_PROTOCOL_PREFIX.len());
    let probe = async {
        let mut byte = [0u8; 1];
        while sniffed.len() < BT_PROTOCOL_PREFIX.len() {
            read.read_exact(&mut byte).await?;
            sniffed.push(byte[0]);
        }
        Ok::<_, std::io::Error>(())
    };
    match tokio::time::timeout(PLAINTEXT_SNIFF_TIMEOUT, probe).await {
        Ok(Ok(())) => {
            let is_plaintext = sniffed == BT_PROTOCOL_PREFIX;
            Ok((is_plaintext.then(|| sniffed.clone()), sniffed))
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_elapsed) => {
            // Window elapsed mid-probe: whatever we got is the leading bytes of
            // the responder's public key (or a slow plaintext peer). Continue MSE.
            Ok((None, sniffed))
        }
    }
}

/// Initiate MSE on a connected stream. IA must be the complete 68-byte
/// BitTorrent handshake and is consumed as part of the MSE exchange.
///
/// Before committing to MSE, briefly probes the responder for a plaintext BT
/// handshake (see [`sniff_plaintext`]); if detected, returns
/// [`OutgoingOutcome::PlaintextPeer`] so the caller can redial plaintext
/// instead of waiting out the full MSE read timeout.
pub async fn outgoing<R, W>(
    mut read: R,
    mut write: W,
    info_hash: &[u8; 20],
    initial_payload: &[u8; BT_HANDSHAKE_LEN],
) -> Result<OutgoingOutcome<R, W>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let dh = Dh768::generate(&mut rand::rng());
    write.write_all(&dh.public_key_bytes()).await?;
    write.write_all(&random_pad(MAX_PAD)).await?;
    trace!("sent MSE Ya + PadA, probing responder");

    // Sniff for a plaintext responder before committing to MSE.
    let (plaintext, sniffed) = sniff_plaintext(&mut read).await?;
    if plaintext.is_some() {
        debug!(
            "peer answered with plaintext BT handshake within sniff window, falling back to plaintext redial"
        );
        return Ok(OutgoingOutcome::PlaintextPeer);
    }

    let mut server_public = [0u8; 96];
    server_public[..sniffed.len()].copy_from_slice(&sniffed);
    read.read_exact(&mut server_public[sniffed.len()..])
        .await
        .context("disconnected waiting for MSE responder public key")?;
    let secret = dh
        .shared_secret(&server_public)
        .ok_or_else(|| anyhow::anyhow!("MSE degenerate remote DH key"))?;
    let (mut encrypt, decrypt_base, decrypt_key) = derive_keys(&secret, info_hash, true);

    write.write_all(&sha1(&[b"req1", &secret])).await?;
    let skey_hash = sha1(&[b"req2", info_hash]);
    let req3 = sha1(&[b"req3", &secret]);
    write.write_all(&xor20(&skey_hash, &req3)).await?;

    let pad_c = random_pad(MAX_PAD);
    let mut encrypted =
        Vec::with_capacity(VC_LEN + 4 + 2 + pad_c.len() + 2 + initial_payload.len());
    encrypted.extend_from_slice(&[0u8; VC_LEN]);
    encrypted.extend_from_slice(&CRYPTO_RC4.to_be_bytes());
    encrypted.extend_from_slice(&u16::try_from(pad_c.len()).expect("pad_c <= MAX_PAD").to_be_bytes());
    encrypted.extend_from_slice(&pad_c);
    encrypted.extend_from_slice(&u16::try_from(BT_HANDSHAKE_LEN).expect("handshake <= 65535").to_be_bytes());
    encrypted.extend_from_slice(initial_payload);
    encrypt.apply_keystream(&mut encrypted);
    write.write_all(&encrypted).await?;

    // PadB is plaintext of unknown length followed by the encrypted VC. Rather
    // than probing each offset with a cloned decrypt state (libtorrent does the
    // same), compute the encrypted-VC pattern once and search the raw bytes for
    // it (matching libtorrent `read_pe_syncvc` / transmission `read_vc`). The
    // VC is the first 8 keystream bytes of the decrypt stream, so after the
    // search `decrypt` continues from exactly the right position.
    let vc_pattern = {
        let mut pattern = [0u8; VC_LEN];
        let mut probe = Rc4::new_from_slice(&decrypt_key).expect("20-byte RC4 key");
        let mut drop = [0u8; 1024];
        probe.apply_keystream(&mut drop);
        probe.apply_keystream(&mut pattern);
        pattern
    };
    let mut raw = Vec::with_capacity(MAX_PAD + VC_LEN);
    while raw.len() < MAX_PAD + VC_LEN {
        let mut byte = [0u8; 1];
        read.read_exact(&mut byte).await?;
        raw.push(byte[0]);
        if raw.len() >= VC_LEN && raw[raw.len() - VC_LEN..] == vc_pattern {
            break;
        }
    }
    if raw.len() < VC_LEN || raw[raw.len() - VC_LEN..] != vc_pattern {
        warn!("MSE verification constant not found within PadB, aborting handshake");
        bail!("MSE verification constant not found within PadB");
    }
    let mut decrypt = decrypt_base;
    // The VC is the first 8 keystream bytes of the decrypt stream. Decrypt the
    // actual received VC ciphertext (raw's tail) and confirm it is the all-zero
    // verification constant, which also advances the stream to the
    // crypto-select / pad-length fields that follow.
    let mut vc = raw[raw.len() - VC_LEN..].to_vec();
    decrypt.apply_keystream(&mut vc);
    if vc != [0u8; VC_LEN] {
        warn!("MSE invalid verification constant after PadB, aborting handshake");
        bail!("MSE invalid verification constant");
    }

    let mut select = [0u8; 4];
    read_encrypted(&mut read, &mut decrypt, &mut select).await?;
    if u32::from_be_bytes(select) != CRYPTO_RC4 {
        warn!("MSE responder did not select RC4, aborting handshake");
        bail!("MSE responder did not select RC4");
    }
    let mut pad_length = [0u8; 2];
    read_encrypted(&mut read, &mut decrypt, &mut pad_length).await?;
    let pad_length = u16::from_be_bytes(pad_length) as usize;
    if pad_length > MAX_PAD {
        bail!("MSE PadD exceeds {MAX_PAD} bytes");
    }
    let mut pad_d = vec![0u8; pad_length];
    read_encrypted(&mut read, &mut decrypt, &mut pad_d).await?;

    trace!("MSE outgoing handshake complete, RC4 established");
    Ok(OutgoingOutcome::Encrypted(
        Rc4Reader::new(read, decrypt),
        Rc4Writer::new(write, encrypt),
    ))
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
    let (mut encrypt, mut decrypt, _decrypt_key) = derive_keys(&secret, &info_hash, false);

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
    response.extend_from_slice(&u16::try_from(pad_d.len()).expect("pad_d <= MAX_PAD").to_be_bytes());
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
