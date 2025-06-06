use anyhow::{Context, Result};
use async_compression::tokio::bufread::GzipDecoder;
use futures::TryStreamExt;
use intervaltree::IntervalTree;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::pin::Pin;
use std::str::FromStr;
use tokio::io::{AsyncBufRead, AsyncRead};
use tokio::{io::AsyncBufReadExt, io::BufReader};
use tokio_util::io::StreamReader;
use tracing::{debug, info, trace};
use url::Url;

pub struct Blocklist {
    // ipv4 and ipv6 do not overlap
    // see: https://www.rfc-editor.org/rfc/rfc4291#section-2.5.5
    blocked_ranges: IntervalTree<IpAddr, ()>,
}

impl Blocklist {
    pub fn empty() -> Self {
        Self::new(std::iter::empty())
    }

    pub fn new(ip_ranges: impl IntoIterator<Item = std::ops::Range<IpAddr>>) -> Self {
        Self {
            blocked_ranges: IntervalTree::from_iter(ip_ranges.into_iter().map(|r| (r, ()))),
        }
    }

    pub async fn load_from_url(url: &str) -> Result<Self> {
        let parsed_url = Url::parse(url).context("Failed to parse URL")?;

        if parsed_url.scheme() == "file" {
            let path = parsed_url
                .to_file_path()
                .ok()
                .context("failed to convert file URL to path")?;
            return Self::load_from_file(path).await;
        }

        let response = reqwest::get(parsed_url)
            .await
            .context("Failed to send request for blocklist")?;
        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch blocklist: HTTP {}", response.status());
        }

        let reader = StreamReader::new(response.bytes_stream().map_err(std::io::Error::other));
        Self::create_from_stream(reader).await
    }

    pub async fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = tokio::fs::File::open(path).await?;
        let reader = tokio::io::BufReader::new(file);
        Self::create_from_stream(reader).await
    }

    async fn create_from_stream<R>(reader: R) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send,
    {
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

        let mut reader: Pin<Box<dyn AsyncBufRead + Send>> = if is_gzip {
            trace!("Detected Gzip file, decompressing...");
            Box::pin(BufReader::new(GzipDecoder::new(reader)))
        } else {
            trace!("Plain text file detected.");
            Box::pin(reader)
        };

        let mut line: String = Default::default();
        let mut ip_ranges: Vec<std::ops::Range<IpAddr>> = Vec::new();
        while reader.read_line(&mut line).await? > 0 {
            if let Some((start_ip, end_ip)) = parse_ip_range(&line) {
                let range = start_ip..increment_ip(end_ip);
                ip_ranges.push(range);
            }
            line.clear();
        }

        info!(
            ip_entry_count = ip_ranges.len(),
            "Finished loading blocklist"
        );

        let blocklist = Self::new(ip_ranges);
        Ok(blocklist)
    }

    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        self.blocked_ranges.query_point(ip).next().is_some()
    }
}

/// Safely increments an `IpAddr`, returning `None` if it would overflow.
fn increment_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(ipv4) => {
            let num = u32::from_be_bytes(ipv4.octets());
            std::net::IpAddr::V4(Ipv4Addr::from(num.saturating_add(1)))
        }
        IpAddr::V6(ipv6) => {
            let num = u128::from_be_bytes(ipv6.octets());
            std::net::IpAddr::V6(Ipv6Addr::from(num.saturating_add(1)))
        }
    }
}

fn parse_ip_range(line: &str) -> Option<(IpAddr, IpAddr)> {
    // Skip comments and empty lines
    let line = line.trim();
    if line.starts_with('#') || line.is_empty() {
        return None;
    }

    let is_ipv4 = line.matches('.').count() >= 6;
    // Find the split point based on whether it's IPv4 or not
    let split_point: usize = if is_ipv4 {
        line.rfind(':')
    } else {
        line.find(':')
    }
    .unwrap_or(0);

    let (rule_name, ip_range) = line.split_at(split_point + 1);
    if let Some((start, end)) = ip_range.split_once('-') {
        if let (Ok(start_ip), Ok(end_ip)) =
            (IpAddr::from_str(start.trim()), IpAddr::from_str(end.trim()))
        {
            return Some((start_ip, end_ip));
        } else {
            // Mismatched IP versions, skip this range
            debug!(rulen_name = rule_name, "Could not be parsed");
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use async_compression::tokio::write::GzipEncoder;
    use futures::stream::once;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_blocklist_gzipped() -> Result<()> {
        let blocklist = r#"
        # test
        local:192.168.1.1-192.168.1.255
        localv6:2001:db8::1-2001:db8::ffff
        "#;
        let mut gzipped_blocklist = Vec::new();
        {
            let mut encoder = GzipEncoder::new(&mut gzipped_blocklist);
            encoder.write_all(blocklist.as_bytes()).await.unwrap();
            encoder.flush().await.unwrap();
            encoder.shutdown().await.unwrap();
        }

        let stream = StreamReader::new(Box::pin(once(async {
            Ok::<_, std::io::Error>(Cursor::new(gzipped_blocklist))
        })));
        let blocklist = Blocklist::create_from_stream(stream).await?;
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("8.8.8.8".parse().unwrap()));

        Ok(())
    }

    #[tokio::test]
    async fn test_blocklist_plaintext() -> Result<()> {
        let blocklist = r#"
        # test
        local:192.168.1.1-192.168.1.255
        localv6:2001:db8::1-2001:db8::ffff
        "#;

        let stream = StreamReader::new(Box::pin(once(async {
            Ok::<_, std::io::Error>(Cursor::new(blocklist.as_bytes().to_vec()))
        })));
        let blocklist = Blocklist::create_from_stream(stream).await?;
        assert!(blocklist.is_blocked("192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked("8.8.8.8".parse().unwrap()));

        Ok(())
    }

    #[tokio::test]
    async fn test_blocklist_from_plaintext_file() -> Result<()> {
        let blocklist_content = r#"
        # test
        local:192.168.1.1-192.168.1.255
        localv6:2001:db8::1-2001:db8::ffff
        "#;

        // Create a temporary file
        let mut temp_file = tokio::fs::File::create("temp_blocklist.txt").await?;
        tokio::io::AsyncWriteExt::write_all(&mut temp_file, blocklist_content.as_bytes()).await?;
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
}
