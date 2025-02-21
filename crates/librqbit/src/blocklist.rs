use anyhow::Result;
use async_compression::tokio::bufread::GzipDecoder;
use futures::TryStreamExt;
use intervaltree::IntervalTree;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::str::FromStr;
use tokio::io::AsyncRead;
use tokio::{io::AsyncBufReadExt, io::BufReader};
use tokio_util::io::StreamReader;
use tracing::{debug, info, trace};

pub struct Blocklist {
    // ipv4 and ipv6 do not overlap
    // see: https://www.rfc-editor.org/rfc/rfc4291#section-2.5.5
    blocked_ranges: IntervalTree<IpAddr, ()>,
}

impl Blocklist {
    pub fn empty() -> Self {
        return Self::new(std::iter::empty());
    }

    pub fn new(ip_ranges: impl IntoIterator<Item = std::ops::Range<IpAddr>>) -> Self {
        Self {
            blocked_ranges: IntervalTree::from_iter(ip_ranges.into_iter().map(|r| (r, ()))),
        }
    }

    pub async fn load_from_url(url: &str) -> Result<Self> {
        let response = reqwest::get(url).await.map_err(|e| anyhow::anyhow!(e))?;
        if response.status() != 200 {
            return Err(anyhow::anyhow!(
                "Failed to fetch blocklist: HTTP {}",
                response.status()
            ));
        }

        let content_length = response
            .content_length()
            .ok_or_else(|| anyhow::anyhow!("Failed to get content length"))?;

        if content_length < 2 {
            return Err(anyhow::anyhow!(
                "Content too short: not enough data to determine compression"
            ));
        }

        let reader = StreamReader::new(
            response
                .bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        );
        Self::create_from_stream(reader).await
    }

    pub async fn load_from_file(path: &str) -> Result<Self> {
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
            return Err(anyhow::anyhow!(
                "Content too short: not enough data to determine compression"
            ));
        }

        // Check for Gzip magic bytes (1F 8B)
        let is_gzip = peek_bytes == [0x1F, 0x8B];

        let reader: Pin<Box<dyn AsyncRead + Send>> = if is_gzip {
            trace!("Detected Gzip file, decompressing...");
            Box::pin(BufReader::new(GzipDecoder::new(reader)))
        } else {
            trace!("Plain text file detected.");
            Box::pin(reader)
        };

        let reader = BufReader::new(reader);
        let mut lines = reader.lines();
        let mut ip_ranges: Vec<std::ops::Range<IpAddr>> = Vec::new();
        while let Some(line) = lines.next_line().await? {
            // Skip comments and empty lines
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }

            if let Some((start_ip, end_ip)) = parse_ip_range(&line) {
                let range = start_ip..(increment_ip(end_ip).unwrap());
                ip_ranges.push(range);
            }
        }

        info!(
            ip_entry_count = ip_ranges.len(),
            "Finished loading blocklist"
        );

        let blocklist = Self::new(ip_ranges);
        Ok(blocklist)
    }

    pub fn is_blocked(&self, ip: &IpAddr) -> bool {
        self.blocked_ranges.query_point(*ip).next().is_some()
    }
}

/// Safely increments an `IpAddr`, returning `None` if it would overflow.
fn increment_ip(ip: IpAddr) -> Option<IpAddr> {
    match ip {
        IpAddr::V4(ipv4) => {
            let num = u32::from_be_bytes(ipv4.octets());
            num.checked_add(1).map(|n| IpAddr::V4(Ipv4Addr::from(n)))
        }
        IpAddr::V6(ipv6) => {
            let num = u128::from_be_bytes(ipv6.octets());
            num.checked_add(1).map(|n| IpAddr::V6(Ipv6Addr::from(n)))
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
    use super::*;
    use async_compression::tokio::write::GzipEncoder;
    use mockito::{Server, ServerGuard};
    use std::thread::{self, JoinHandle};
    use tokio::io::AsyncWriteExt;

    struct TestServer {
        server: ServerGuard,
        mock: mockito::Mock,
        url: String,
        _thread: JoinHandle<()>,
    }

    impl TestServer {
        fn new(content: &[u8], headers: &[(&str, &str)]) -> Self {
            let (tx, rx) = std::sync::mpsc::channel();
            let server_thread = thread::spawn(move || {
                let mut server = Server::new();
                let url = server.url();
                let mock = server.mock("GET", "/").with_status(200);

                tx.send((server, mock, url)).unwrap();
                thread::park();
            });

            let (server, mut mock, url) = rx.recv().unwrap();

            mock = mock.with_body(content);
            for &(key, value) in headers {
                mock = mock.with_header(key, value);
            }
            let mock = mock.create();

            TestServer {
                server,
                mock,
                url,
                _thread: server_thread,
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self._thread.thread().unpark();
        }
    }

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

        let server = TestServer::new(&gzipped_blocklist, &[("Content-Encoding", "gzip")]);

        let blocklist = Blocklist::load_from_url(&server.url).await?;
        assert!(blocklist.is_blocked(&"192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"8.8.8.8".parse().unwrap()));

        server.mock.assert();
        Ok(())
    }

    #[tokio::test]
    async fn test_blocklist_plaintext() -> Result<()> {
        let blocklist = r#"
        # test
        local:192.168.1.1-192.168.1.255
        localv6:2001:db8::1-2001:db8::ffff
        "#;

        let server = TestServer::new(blocklist.as_bytes(), &[]);

        let blocklist = Blocklist::load_from_url(&server.url).await?;
        assert!(blocklist.is_blocked(&"192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"8.8.8.8".parse().unwrap()));

        server.mock.assert();
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
        assert!(blocklist.is_blocked(&"192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"8.8.8.8".parse().unwrap()));
        assert!(blocklist.is_blocked(&"2001:db8::1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"2001:4860:4860::8888".parse().unwrap()));

        // Clean up the temporary file
        tokio::fs::remove_file("temp_blocklist.txt").await?;

        Ok(())
    }

    #[test]
    fn test_blocklist_empty() {
        let blocklist = Blocklist::empty();
        assert!(!blocklist.is_blocked(&"127.0.0.1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"::1".parse().unwrap()));
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
        assert!(blocklist.is_blocked(&"192.168.1.1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"10.0.0.1".parse().unwrap()));

        // Test IPv6 addresses
        assert!(blocklist.is_blocked(&"2001:db8::1".parse().unwrap()));
        assert!(!blocklist.is_blocked(&"2001:db9::1".parse().unwrap()));
    }
}
