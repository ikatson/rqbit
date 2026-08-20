//! Message Stream Encryption (MSE) support.
//!
//! MSE obfuscates the BitTorrent handshake and stream with RC4 and a DH-768
//! key exchange (Azureus protocol encryption). Some peers (Xunlei, BitComet,
//! etc.) refuse plaintext handshakes, so MSE is required to talk to them.
//!
//! This module is built top-down: the [`MseMode`] config and the peer
//! connection wiring land first, and the handshake implementations are filled
//! in by follow-up commits.

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

/// Accept either a complete plaintext BitTorrent handshake or MSE.
///
/// Not yet implemented: this placeholder is wired into the incoming peer
/// connection path and will be replaced by the full handshake.
pub async fn incoming<R, W, F>(read: R, write: W, lookup: F) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
    F: Fn(&[u8; 20]) -> Option<[u8; 20]>,
{
    let _ = (read, write, lookup);
    anyhow::bail!("MSE incoming handshake not implemented")
}
