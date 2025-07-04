use anyhow::{Context, Result};
use async_compression::tokio::bufread::GzipDecoder;
use futures::TryStreamExt;
use intervaltree::IntervalTree;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::str::FromStr;
use tokio::io::{AsyncBufRead, AsyncRead};
use tokio::{io::AsyncBufReadExt, io::BufReader};
use tokio_util::io::StreamReader;
use tracing::trace;
use url::Url;

pub struct Blocklist {
    // ipv4 and ipv6 do not overlap
    // see: https://www.rfc-editor.org/rfc/rfc4291#section-2.5.5
    blocked_ranges: IntervalTree<IpAddr, ()>,
    len: usize,
}

impl Blocklist {
    pub fn empty() -> Self {
        Self::new(std::iter::empty())
    }

    pub fn new(ip_ranges: impl IntoIterator<Item = std::ops::Range<IpAddr>>) -> Self {
        let mut len = 0;
        let it = ip_ranges.into_iter().map(|r| {
            len += 1;
            (r, ())
        });
        Self {
            blocked_ranges: IntervalTree::from_iter(it),
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub async fn load_from_url(url: &str) -> Result<Self> {
        let parsed_url = Url::parse(url).context("failed to parse URL")?;

        if parsed_url.scheme() == "file" {
            let path = parsed_url
                .to_file_path()
                .ok()
                .context("failed to convert file URL to path")?;
            return Self::load_from_file(path).await;
        }

        let response = reqwest::get(parsed_url)
            .await
            .context("error fetching blocklist")?;
        if !response.status().is_success() {
            anyhow::bail!("error fetching blocklist: HTTP {}", response.status());
        }

        let mut reader = StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
        let bl = Self::create_from_stream(&mut reader).await?;
        Ok(bl)
    }

    pub async fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut file = tokio::fs::File::open(path).await?;
        Self::create_from_stream(&mut file).await
    }

    async fn create_from_stream(reader: &mut (dyn AsyncRead + Unpin + Send)) -> Result<Self> {
        let mut peek_bytes = [0u8; 2];
        let mut reader = tokio::io::BufReader::new(reader);

        // Peek the first bytes by filling buffer
        let buffer = reader.fill_buf().await?;
        if buffer.len() >= 2 {
            peek_bytes.copy_from_slice(&buffer[0..2]);
        } else {
            anyhow::bail!("Content too short: not enough data to determine compression");
        }

        // Check for Gzip magic bytes (1F 8B)
        let is_gzip = peek_bytes == [0x1F, 0x8B];

        if is_gzip {
            trace!("Detected Gzip file, decompressing...");
            Self::create_from_decoded_stream(&mut BufReader::new(GzipDecoder::new(reader))).await
        } else {
            trace!("Plain text file detected.");
            Self::create_from_decoded_stream(&mut reader).await
        }
    }

    async fn create_from_decoded_stream(
        reader: &mut (dyn AsyncBufRead + Unpin + Send),
    ) -> Result<Self> {
        let mut line: String = Default::default();
        let mut ip_ranges: Vec<std::ops::Range<IpAddr>> = Vec::new();
        while reader.read_line(&mut line).await? > 0 {
            if let Some((start_ip, end_ip)) = parse_ip_range(&line) {
                let range = start_ip..increment_ip(end_ip);
                ip_ranges.push(range);
            } else {
                tracing::debug!(line, "couldn't parse line");
            }
            line.clear();
        }

        let blocklist = Self::new(ip_ranges);
        Ok(blocklist)
    }

    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        self.blocked_ranges.query_point(ip).next().is_some()
    }
}

/// Safely increments an `IpAddr`, as IntervalTree doesn't support inclusive ranges.
fn increment_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(ipv4) => IpAddr::V4(Ipv4Addr::from(ipv4.to_bits().saturating_add(1))),
        IpAddr::V6(ipv6) => IpAddr::V6(Ipv6Addr::from(ipv6.to_bits().saturating_add(1))),
    }
}

fn parse_ip_range(line: &str) -> Option<(IpAddr, IpAddr)> {
    let line = line.trim();
    if line.starts_with('#') || line.is_empty() {
        return None;
    }

    let (_name, ips) = {
        let is_ipv6 = line.matches(":").count() > 2;
        if is_ipv6 {
            line.split_once(':')?
        } else {
            line.rsplit_once(':')?
        }
    };
    let (start, end) = ips.split_once('-')?;
    match (IpAddr::from_str(start).ok()?, IpAddr::from_str(end).ok()?) {
        (start @ IpAddr::V4(_), end @ IpAddr::V4(_))
        | (start @ IpAddr::V6(_), end @ IpAddr::V6(_)) => Some((start, end)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::tests::test_util::setup_test_logging;

    use super::*;
    use async_compression::tokio::write::GzipEncoder;
    use tokio::io::AsyncWriteExt;

    const BLOCKLIST: &[u8] = br#"
    # test
    local:192.168.1.1-192.168.1.255
    localv6:2001:db8::1-2001:db8::ffff
    "#;

    #[tokio::test]
    async fn test_blocklist_gzipped() -> Result<()> {
        let mut gzipped_blocklist = Vec::new();
        {
            let mut encoder = GzipEncoder::new(&mut gzipped_blocklist);
            encoder.write_all(BLOCKLIST).await.unwrap();
            encoder.flush().await.unwrap();
            encoder.shutdown().await.unwrap();
        }
        let blocklist = Blocklist::create_from_stream(&mut Cursor::new(gzipped_blocklist)).await?;
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("8.8.8.8".parse().unwrap()));

        Ok(())
    }

    #[tokio::test]
    async fn test_blocklist_plaintext() -> Result<()> {
        let blocklist = Blocklist::create_from_stream(&mut Cursor::new(BLOCKLIST)).await?;
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("8.8.8.8".parse().unwrap()));

        Ok(())
    }

    #[tokio::test]
    async fn test_blocklist_from_plaintext_file() -> Result<()> {
        // Create a temporary file
        let mut temp_file = tokio::fs::File::create("temp_blocklist.txt").await?;
        tokio::io::AsyncWriteExt::write_all(&mut temp_file, BLOCKLIST).await?;
        drop(temp_file); // Close the file

        // Load the blocklist from the file
        let blocklist = Blocklist::load_from_file("temp_blocklist.txt").await?;

        // Verify the blocklist
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("8.8.8.8".parse().unwrap()));
        assert!(blocklist.is_blocked("2001:db8::1".parse().unwrap()));
        assert!(!blocklist.is_blocked("2001:4860:4860::8888".parse().unwrap()));

        // Clean up the temporary file
        tokio::fs::remove_file("temp_blocklist.txt").await?;

        Ok(())
    }

    #[test]
    fn test_blocklist_empty() {
        let blocklist = Blocklist::empty();
        assert!(!blocklist.is_blocked("127.0.0.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("::1".parse().unwrap()));
    }

    #[test]
    fn test_manual_ranges() {
        // Add IPv4 range
        let start_v4: IpAddr = "192.168.0.0".parse().unwrap();
        let end_v4: IpAddr = "192.168.255.255".parse().unwrap();
        let ipv4_range = start_v4..end_v4;

        // Add IPv6 range
        let start_v6: IpAddr = "2001:db8::".parse().unwrap();
        let end_v6: IpAddr = "2001:db8::ffff".parse().unwrap();
        let ipv6_range = start_v6..end_v6;

        let blocklist = Blocklist::new(vec![ipv4_range, ipv6_range]);
        // Test IPv4 addresses
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("10.0.0.1".parse().unwrap()));

        // Test IPv6 addresses
        assert!(blocklist.is_blocked("2001:db8::1".parse().unwrap()));
        assert!(!blocklist.is_blocked("2001:db9::1".parse().unwrap()));
    }

    #[ignore]
    #[tokio::test]
    async fn test_blocklist_real_url() {
        setup_test_logging();
        let _ = Blocklist::load_from_url("https://raw.githubusercontent.com/Naunter/BT_BlockLists/refs/heads/master/bt_blocklists.gz")
            .await
            .unwrap();
    }
}
