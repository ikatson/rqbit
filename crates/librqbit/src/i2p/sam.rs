//! The client side of the SAM v3.1 text protocol (<https://geti2p.net/en/docs/api/samv3>).
//!
//! Every connection to the bridge starts with `HELLO VERSION`. One connection carries the
//! `SESSION CREATE` and lives as long as the session; every stream (`STREAM CONNECT`,
//! `STREAM ACCEPT`) needs a connection of its own, which becomes the data stream once the
//! bridge said `RESULT=OK`. Replies are single lines of `key=value` tokens after two words
//! naming the reply.

use std::net::SocketAddr;

use librqbit_core::i2p::I2pDestination;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};

/// Lines longer than this are an error. The longest legitimate ones carry a private key
/// (`SESSION STATUS`) or a destination with a certificate, well below 2 KiB.
pub(crate) const MAX_LINE_LEN: usize = 4096;

/// The only protocol version spoken.
pub(crate) const SAM_VERSION: &str = "3.1";

/// What a `RESULT=` that is not `OK` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SamResultCode {
    CantReachPeer,
    DuplicatedDest,
    DuplicatedId,
    I2pError,
    InvalidId,
    InvalidKey,
    KeyNotFound,
    PeerNotFound,
    Timeout,
    NoVersion,
    AlreadyAccepting,
    Other(String),
}

impl SamResultCode {
    fn parse(code: &str) -> Self {
        match code {
            "CANT_REACH_PEER" => Self::CantReachPeer,
            "DUPLICATED_DEST" => Self::DuplicatedDest,
            "DUPLICATED_ID" => Self::DuplicatedId,
            "I2P_ERROR" => Self::I2pError,
            "INVALID_ID" => Self::InvalidId,
            "INVALID_KEY" => Self::InvalidKey,
            "KEY_NOT_FOUND" => Self::KeyNotFound,
            "PEER_NOT_FOUND" => Self::PeerNotFound,
            "TIMEOUT" => Self::Timeout,
            "NOVERSION" => Self::NoVersion,
            "ALREADY_ACCEPTING" => Self::AlreadyAccepting,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl std::fmt::Display for SamResultCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CantReachPeer => "the peer can't be reached (CANT_REACH_PEER)",
            Self::DuplicatedDest => "the destination is in use already (DUPLICATED_DEST)",
            Self::DuplicatedId => "the session id is in use already (DUPLICATED_ID)",
            Self::I2pError => "generic I2P error (I2P_ERROR)",
            Self::InvalidId => "unknown session id (INVALID_ID)",
            Self::InvalidKey => "invalid destination (INVALID_KEY)",
            Self::KeyNotFound => "the name is not known (KEY_NOT_FOUND)",
            Self::PeerNotFound => "the peer was not found (PEER_NOT_FOUND)",
            Self::Timeout => "timed out (TIMEOUT)",
            Self::NoVersion => "the bridge does not speak SAM 3.1 (NOVERSION)",
            Self::AlreadyAccepting => "a STREAM ACCEPT is pending already (ALREADY_ACCEPTING)",
            Self::Other(code) => return write!(f, "error {code:?}"),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SamError {
    #[error("SAM bridge I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SAM bridge sent a line longer than {MAX_LINE_LEN} bytes")]
    LineTooLong,
    #[error("SAM bridge sent a malformed line: {0}")]
    Malformed(String),
    #[error("SAM bridge sent {got:?} instead of {expected}")]
    Unexpected { expected: &'static str, got: String },
    #[error("SAM bridge: {code}{}", .message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Result {
        code: SamResultCode,
        message: Option<String>,
    },
    #[error("can't send {0:?} to the SAM bridge: it contains a line break or a NUL")]
    BadValue(String),
}

/// One reply line: two words, then `key=value` tokens.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    pub topic: String,
    pub kind: String,
    pairs: Vec<(String, String)>,
}

impl Reply {
    /// Parses a line (without its line break). Values may be quoted (`"a \"b\" c"`); a key
    /// that appears twice, a token without `=` and anything after a closing quote are errors.
    pub fn parse(line: &str) -> Result<Self, SamError> {
        let malformed = |why: &str| SamError::Malformed(format!("{why}: {line:?}"));
        let mut rest = line.trim_end_matches([' ', '\t']);
        let mut word = || -> Option<String> {
            rest = rest.trim_start_matches(' ');
            let end = rest.find(' ').unwrap_or(rest.len());
            let (w, r) = rest.split_at(end);
            rest = r;
            (!w.is_empty() && !w.contains('=')).then(|| w.to_owned())
        };
        let topic = word().ok_or_else(|| malformed("no reply name"))?;
        let kind = word().ok_or_else(|| malformed("no reply kind"))?;

        let mut pairs: Vec<(String, String)> = Vec::new();
        loop {
            rest = rest.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
            let eq = rest
                .find('=')
                .ok_or_else(|| malformed("a token without '='"))?;
            let key = &rest[..eq];
            if key.is_empty() || key.contains(' ') || key.contains('"') {
                return Err(malformed("bad key"));
            }
            rest = &rest[eq + 1..];
            let value = if let Some(quoted) = rest.strip_prefix('"') {
                let mut value = String::new();
                let mut chars = quoted.char_indices();
                let end = loop {
                    match chars.next() {
                        None => return Err(malformed("unterminated quote")),
                        Some((i, '"')) => break i,
                        Some((_, '\\')) => match chars.next() {
                            Some((_, c @ ('"' | '\\'))) => value.push(c),
                            _ => return Err(malformed("bad escape")),
                        },
                        Some((_, c)) => value.push(c),
                    }
                };
                rest = &quoted[end + 1..];
                if !(rest.is_empty() || rest.starts_with(' ')) {
                    return Err(malformed("text after a closing quote"));
                }
                value
            } else {
                let end = rest.find(' ').unwrap_or(rest.len());
                let (v, r) = rest.split_at(end);
                if v.contains('"') {
                    return Err(malformed("a quote inside an unquoted value"));
                }
                rest = r;
                v.to_owned()
            };
            if pairs.iter().any(|(k, _)| k == key) {
                return Err(malformed("duplicate key"));
            }
            pairs.push((key.to_owned(), value));
        }
        Ok(Self { topic, kind, pairs })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every `key=value` token, in order.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Checks that this is the `topic kind` reply and that it says `RESULT=OK`.
    pub fn expect_ok(self, topic: &str, kind: &str) -> Result<Self, SamError> {
        if self.topic != topic || self.kind != kind {
            return Err(SamError::Unexpected {
                expected: "the reply to the command",
                got: format!("{} {}", self.topic, self.kind),
            });
        }
        match self.get("RESULT") {
            Some("OK") => Ok(self),
            Some(code) => Err(SamError::Result {
                code: SamResultCode::parse(code),
                message: self.get("MESSAGE").map(str::to_owned),
            }),
            None => Err(SamError::Malformed(format!(
                "{topic} {kind} without RESULT"
            ))),
        }
    }
}

/// Formats a command line: the words, then `key=value` pairs, quoting values that need it,
/// then the line break.
pub(crate) fn command(words: &str, pairs: &[(&str, &str)]) -> Result<String, SamError> {
    let mut line = String::from(words);
    for (key, value) in pairs {
        if value.contains(['\n', '\r', '\0']) {
            return Err(SamError::BadValue((*value).to_owned()));
        }
        line.push(' ');
        line.push_str(key);
        line.push('=');
        if value.is_empty() || value.contains([' ', '\t', '"', '\\']) {
            line.push('"');
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    line.push('\\');
                }
                line.push(c);
            }
            line.push('"');
        } else {
            line.push_str(value);
        }
    }
    line.push('\n');
    Ok(line)
}

/// Reads one line, without its line break (`\n`, or `\r\n`). Never reads past the line
/// break: what follows belongs to the stream once a `STREAM` command succeeded.
pub(crate) async fn read_line(r: &mut (impl AsyncBufRead + Unpin)) -> Result<String, SamError> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Err(SamError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the SAM bridge closed the connection",
            )));
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if line.len() + take > MAX_LINE_LEN + 1 {
            return Err(SamError::LineTooLong);
        }
        line.extend_from_slice(&buf[..take]);
        r.consume(take);
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return String::from_utf8(line)
                .map_err(|_| SamError::Malformed("a line that is not UTF-8".to_owned()));
        }
    }
}

/// The options of `SESSION CREATE`, besides the style, the id and the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TunnelOptions {
    pub inbound_quantity: u8,
    pub outbound_quantity: u8,
    pub inbound_length: u8,
    pub outbound_length: u8,
}

/// The signature type of transient destinations: Ed25519 (`SIGNATURE_TYPE=7`).
pub(crate) const SIGNATURE_TYPE: &str = "7";

/// A connection to the bridge, after `HELLO`.
pub(crate) struct SamConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

/// The read half of a stream: the bridge's buffered reader, which may hold the first bytes of
/// the peer already.
pub(crate) type SamReadHalf = BufReader<OwnedReadHalf>;

impl SamConn {
    /// Connects and negotiates SAM 3.1.
    pub async fn open(addr: SocketAddr) -> Result<Self, SamError> {
        let stream = TcpStream::connect(addr).await?;
        // Commands are small and every one waits for its reply.
        stream.set_nodelay(true)?;
        let (r, w) = stream.into_split();
        let mut conn = Self {
            reader: BufReader::new(r),
            writer: w,
        };
        let reply = conn
            .request(
                "HELLO VERSION",
                &[("MIN", SAM_VERSION), ("MAX", SAM_VERSION)],
            )
            .await?
            .expect_ok("HELLO", "REPLY")?;
        match reply.get("VERSION") {
            Some(SAM_VERSION) => Ok(conn),
            other => Err(SamError::Malformed(format!(
                "HELLO REPLY with version {other:?}"
            ))),
        }
    }

    async fn request(&mut self, words: &str, pairs: &[(&str, &str)]) -> Result<Reply, SamError> {
        let line = command(words, pairs)?;
        self.writer.write_all(line.as_bytes()).await?;
        Reply::parse(&read_line(&mut self.reader).await?)
    }

    /// `SESSION CREATE STYLE=STREAM` with a transient destination. Returns the private key
    /// the bridge generated (base64); the connection must stay open for the session to live.
    pub async fn session_create(
        &mut self,
        id: &str,
        tunnels: &TunnelOptions,
    ) -> Result<String, SamError> {
        let (iq, oq, il, ol) = (
            tunnels.inbound_quantity.to_string(),
            tunnels.outbound_quantity.to_string(),
            tunnels.inbound_length.to_string(),
            tunnels.outbound_length.to_string(),
        );
        let reply = self
            .request(
                "SESSION CREATE",
                &[
                    ("STYLE", "STREAM"),
                    ("ID", id),
                    ("DESTINATION", "TRANSIENT"),
                    ("SIGNATURE_TYPE", SIGNATURE_TYPE),
                    ("inbound.quantity", &iq),
                    ("outbound.quantity", &oq),
                    ("inbound.length", &il),
                    ("outbound.length", &ol),
                ],
            )
            .await?
            .expect_ok("SESSION", "STATUS")?;
        reply
            .get("DESTINATION")
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| SamError::Malformed("SESSION STATUS without DESTINATION".to_owned()))
    }

    /// `NAMING LOOKUP`. `ME` is the destination of the session of this connection.
    pub async fn naming_lookup(&mut self, name: &str) -> Result<I2pDestination, SamError> {
        let reply = self
            .request("NAMING LOOKUP", &[("NAME", name)])
            .await?
            .expect_ok("NAMING", "REPLY")?;
        if reply.get("NAME") != Some(name) {
            return Err(SamError::Malformed(format!(
                "NAMING REPLY for {:?} instead of {name:?}",
                reply.get("NAME")
            )));
        }
        let value = reply
            .get("VALUE")
            .ok_or_else(|| SamError::Malformed("NAMING REPLY without VALUE".to_owned()))?;
        I2pDestination::from_i2p_base64(value)
            .map_err(|e| SamError::Malformed(format!("NAMING REPLY with a bad VALUE: {e:#}")))
    }

    /// `STREAM CONNECT`: the connection becomes a stream to `dest`.
    pub async fn stream_connect(
        mut self,
        id: &str,
        dest: &I2pDestination,
    ) -> Result<(SamReadHalf, OwnedWriteHalf), SamError> {
        self.request(
            "STREAM CONNECT",
            &[
                ("ID", id),
                ("DESTINATION", &dest.to_i2p_base64()),
                ("SILENT", "false"),
            ],
        )
        .await?
        .expect_ok("STREAM", "STATUS")?;
        Ok((self.reader, self.writer))
    }

    /// `STREAM ACCEPT`: once the bridge said OK, waits for a peer; see
    /// [`PendingAccept::peer`].
    pub async fn stream_accept(mut self, id: &str) -> Result<PendingAccept, SamError> {
        self.request("STREAM ACCEPT", &[("ID", id), ("SILENT", "false")])
            .await?
            .expect_ok("STREAM", "STATUS")?;
        Ok(PendingAccept { conn: self })
    }

    /// Reads whatever comes, e.g. to notice that the session connection was closed. SAM 3.1
    /// sends nothing on it; lines are ignored.
    pub async fn wait_closed(mut self) -> SamError {
        loop {
            match read_line(&mut self.reader).await {
                Ok(line) => tracing::debug!(?line, "unexpected line on the SAM session connection"),
                Err(e) => return e,
            }
        }
    }
}

/// A `STREAM ACCEPT` the bridge took.
pub(crate) struct PendingAccept {
    conn: SamConn,
}

impl PendingAccept {
    /// Waits for a peer: the bridge sends its destination on a line, then the stream starts.
    pub async fn peer(mut self) -> Result<(I2pDestination, SamReadHalf, OwnedWriteHalf), SamError> {
        let line = read_line(&mut self.conn.reader).await?;
        // SAM 3.2 adds `FROM_PORT=… TO_PORT=…` after the destination; 3.1 sends it alone.
        let dest = line.split(' ').next().unwrap_or_default();
        if line.starts_with("STREAM STATUS") {
            // The bridge gave up on the accept after all.
            Reply::parse(&line)?.expect_ok("STREAM", "STATUS")?;
            return Err(SamError::Unexpected {
                expected: "a destination",
                got: line,
            });
        }
        let dest = I2pDestination::from_i2p_base64(dest)
            .map_err(|e| SamError::Malformed(format!("bad accepted destination: {e:#}")))?;
        Ok((dest, self.conn.reader, self.conn.writer))
    }
}

/// A fresh session id: unique per session, as the bridge refuses ids in use.
pub(crate) fn new_session_id() -> String {
    format!("rqbit-{}", hex::encode(rand::random::<[u8; 6]>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Reply, SamError> {
        Reply::parse(line)
    }

    #[test]
    fn replies() {
        let r = parse("HELLO REPLY RESULT=OK VERSION=3.1").unwrap();
        assert_eq!((r.topic.as_str(), r.kind.as_str()), ("HELLO", "REPLY"));
        assert_eq!(r.get("VERSION"), Some("3.1"));
        assert_eq!(r.get("MISSING"), None);

        // Quoted values with escapes, unknown keys, empty values, trailing blanks.
        let r = parse(
            r#"STREAM STATUS RESULT=I2P_ERROR MESSAGE="Can't \"route\" to \\ peer" FUTURE=x EMPTY= "#,
        )
        .unwrap();
        assert_eq!(r.get("MESSAGE"), Some(r#"Can't "route" to \ peer"#));
        assert_eq!(r.get("FUTURE"), Some("x"));
        assert_eq!(r.get("EMPTY"), Some(""));
        // Base64 padding inside a value.
        let r = parse("NAMING REPLY RESULT=OK NAME=ME VALUE=abc==").unwrap();
        assert_eq!(r.get("VALUE"), Some("abc=="));
    }

    #[test]
    fn malformed_replies() {
        for line in [
            "",
            "HELLO",
            "HELLO RESULT=OK",
            "HELLO REPLY RESULT",
            "HELLO REPLY =OK",
            "HELLO REPLY RESULT=OK RESULT=OK",
            r#"HELLO REPLY MESSAGE="unterminated"#,
            r#"HELLO REPLY MESSAGE="a"b"#,
            r#"HELLO REPLY MESSAGE="bad \x escape""#,
            r#"HELLO REPLY MESSAGE=a"b"#,
        ] {
            assert!(
                matches!(parse(line), Err(SamError::Malformed(_))),
                "{line:?}"
            );
        }
    }

    #[test]
    fn results_are_mapped() {
        let cases = [
            ("CANT_REACH_PEER", SamResultCode::CantReachPeer),
            ("DUPLICATED_DEST", SamResultCode::DuplicatedDest),
            ("DUPLICATED_ID", SamResultCode::DuplicatedId),
            ("I2P_ERROR", SamResultCode::I2pError),
            ("INVALID_ID", SamResultCode::InvalidId),
            ("INVALID_KEY", SamResultCode::InvalidKey),
            ("KEY_NOT_FOUND", SamResultCode::KeyNotFound),
            ("PEER_NOT_FOUND", SamResultCode::PeerNotFound),
            ("TIMEOUT", SamResultCode::Timeout),
            ("NOVERSION", SamResultCode::NoVersion),
            ("ALREADY_ACCEPTING", SamResultCode::AlreadyAccepting),
            (
                "SOMETHING_NEW",
                SamResultCode::Other("SOMETHING_NEW".into()),
            ),
        ];
        for (text, code) in cases {
            let err = parse(&format!("STREAM STATUS RESULT={text} MESSAGE=\"why not\""))
                .unwrap()
                .expect_ok("STREAM", "STATUS")
                .unwrap_err();
            match err {
                SamError::Result { code: c, message } => {
                    assert_eq!(c, code);
                    assert_eq!(message.as_deref(), Some("why not"));
                }
                other => panic!("{other:?}"),
            }
        }
        // The wrong reply, or none of RESULT.
        assert!(matches!(
            parse("SESSION STATUS RESULT=OK")
                .unwrap()
                .expect_ok("STREAM", "STATUS"),
            Err(SamError::Unexpected { .. })
        ));
        assert!(matches!(
            parse("STREAM STATUS")
                .unwrap()
                .expect_ok("STREAM", "STATUS"),
            Err(SamError::Malformed(_))
        ));
    }

    #[test]
    fn commands() {
        assert_eq!(
            command("HELLO VERSION", &[("MIN", "3.1"), ("MAX", "3.1")]).unwrap(),
            "HELLO VERSION MIN=3.1 MAX=3.1\n"
        );
        assert_eq!(
            command("NAMING LOOKUP", &[("NAME", r#"a "b" \c"#), ("X", "")]).unwrap(),
            "NAMING LOOKUP NAME=\"a \\\"b\\\" \\\\c\" X=\"\"\n"
        );
        // What a command quotes, a reply parses back.
        let line = command("NAMING REPLY", &[("NAME", r#"a "b" \c"#)]).unwrap();
        let r = parse(line.trim_end()).unwrap();
        assert_eq!(r.get("NAME"), Some(r#"a "b" \c"#));
        assert!(matches!(
            command("NAMING LOOKUP", &[("NAME", "a\nSESSION CREATE")]),
            Err(SamError::BadValue(_))
        ));
    }

    #[tokio::test]
    async fn lines() {
        async fn read_all(input: &[u8]) -> Vec<Result<String, String>> {
            let mut r = BufReader::with_capacity(7, input);
            let mut out = Vec::new();
            loop {
                match read_line(&mut r).await {
                    Ok(l) => out.push(Ok(l)),
                    Err(SamError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return out;
                    }
                    Err(e) => {
                        out.push(Err(e.to_string()));
                        return out;
                    }
                }
            }
        }
        assert_eq!(
            read_all(b"HELLO REPLY RESULT=OK\r\nSTREAM STATUS RESULT=OK\n").await,
            vec![
                Ok("HELLO REPLY RESULT=OK".to_owned()),
                Ok("STREAM STATUS RESULT=OK".to_owned())
            ]
        );
        // A line that ends without its line break is not a line.
        assert_eq!(read_all(b"HELLO REPLY").await, vec![]);
        // The longest line passes, one byte more is an error.
        let mut long = vec![b'a'; MAX_LINE_LEN];
        long.push(b'\n');
        assert_eq!(read_all(&long).await.len(), 1);
        let mut long = vec![b'a'; MAX_LINE_LEN + 1];
        long.push(b'\n');
        assert!(read_all(&long).await[0].is_err());
        assert!(read_all(b"\xff\xfe\n").await[0].is_err());
    }

    #[tokio::test]
    async fn the_line_reader_leaves_the_stream_alone() {
        let input: &[u8] = b"STREAM STATUS RESULT=OK\n\x13BitTorrent protocol";
        let mut r = BufReader::new(input);
        assert_eq!(read_line(&mut r).await.unwrap(), "STREAM STATUS RESULT=OK");
        let mut rest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut r, &mut rest)
            .await
            .unwrap();
        assert_eq!(rest, b"\x13BitTorrent protocol");
    }

    #[test]
    fn session_ids_are_unique() {
        let a = new_session_id();
        assert!(a.starts_with("rqbit-") && a.len() == 18, "{a}");
        assert_ne!(a, new_session_id());
    }
}
