use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use postgres::{Client, Config, CopyInWriter, NoTls};
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(about = "Stream rqbit JSON logs into PostgreSQL")]
struct Args {
    /// PostgreSQL connection string.
    #[arg(long, env = "DATABASE_URL", default_value = "postgresql:///rqbit-log")]
    database_url: String,

    /// Log file to load. This is read line-by-line.
    #[arg(long, default_value = "/tmp/rqbit-log")]
    file: PathBuf,

    /// Destination table. Schema-qualified names are supported.
    #[arg(long, default_value = "rqbit_logs")]
    table: String,

    /// Create useful indexes after loading.
    #[arg(long)]
    create_indexes: bool,

    /// Fail on malformed JSON lines instead of skipping them.
    #[arg(long)]
    fail_invalid: bool,
}

struct Stats {
    loaded: u64,
    skipped: u64,
}

struct LogRow {
    timestamp: Option<String>,
    level: Option<String>,
    target: Option<String>,
    message: Option<String>,
    torrent_id: Option<u16>,
    peer: Option<String>,
    peer_port: Option<u16>,
    peer_kind: Option<&'static str>,
    span: Option<String>,
    spans: Option<String>,
    fields: String,
    raw: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let table = SqlTableName::parse(&args.table);

    let mut client = connect(&args.database_url)?;

    recreate_table(&mut client, &table)?;

    let stats = load_log(&mut client, &table, &args.file, args.fail_invalid)?;

    if args.create_indexes {
        create_indexes(&mut client, &table)?;
    }
    create_views(&mut client, &table)?;

    println!(
        "loaded {} rows into {} from {} (skipped {})",
        stats.loaded,
        table.quoted(),
        args.file.display(),
        stats.skipped
    );

    Ok(())
}

fn connect(database_url: &str) -> Result<Client> {
    if let Some(dbname) = database_url.strip_prefix("postgresql:///") {
        return Config::new()
            .host_path("/tmp")
            .dbname(dbname)
            .connect(NoTls)
            .with_context(|| format!("failed to connect to database {dbname:?} via local socket"));
    }

    Client::connect(database_url, NoTls)
        .with_context(|| format!("failed to connect to {database_url:?}"))
}

fn recreate_table(client: &mut Client, table: &SqlTableName) -> Result<()> {
    client
        .batch_execute(&format!(
            r#"
DROP VIEW IF EXISTS error_managing_peer;
DROP TABLE IF EXISTS {};

DO $$
BEGIN
    CREATE TYPE log_level AS ENUM ('TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR');
EXCEPTION WHEN duplicate_object THEN
    NULL;
END $$;

DO $$
BEGIN
    CREATE TYPE peer_kind AS ENUM ('tcp', 'utp', 'socks');
EXCEPTION WHEN duplicate_object THEN
    NULL;
END $$;

CREATE TABLE {} (
    id BIGSERIAL PRIMARY KEY,
    timestamp TIMESTAMPTZ,
    level log_level,
    target TEXT,
    message TEXT,
    torrent_id SMALLINT,
    peer INET,
    peer_port INTEGER,
    peer_kind peer_kind,
    span JSONB,
    spans JSONB,
    fields JSONB NOT NULL,
    raw JSONB NOT NULL
);
"#,
            table.quoted(),
            table.quoted()
        ))
        .with_context(|| format!("failed to recreate table {}", table.quoted()))?;
    Ok(())
}

fn create_views(client: &mut Client, table: &SqlTableName) -> Result<()> {
    client
        .batch_execute(&format!(
            r#"
CREATE OR REPLACE VIEW error_managing_peer AS
WITH errors AS (
    SELECT l.*
    FROM {table} l
    WHERE l.peer IS NOT NULL
      AND l.level = 'ERROR'::log_level
      AND l.message LIKE '%manage_peer finished with error:%'
)
SELECT
    e.id AS error_id,
    e.timestamp AS error_at,
    e.torrent_id,
    e.peer,
    e.peer_port,
    e.peer_kind AS error_peer_kind,
    e.message AS error_message,
    ctx.rn AS msg_before_error,
    ctx.id AS msg_id,
    ctx.timestamp AS msg_at,
    ctx.level,
    ctx.target,
    ctx.peer_kind,
    ctx.message
FROM errors e
CROSS JOIN LATERAL (
    SELECT *
    FROM (
        SELECT
            l.*,
            row_number() OVER (ORDER BY l.timestamp DESC, l.id DESC) - 1 AS rn
        FROM {table} l
        WHERE l.torrent_id IS NOT DISTINCT FROM e.torrent_id
          AND l.peer IS NOT DISTINCT FROM e.peer
          AND l.peer_port IS NOT DISTINCT FROM e.peer_port
          AND (l.timestamp, l.id) <= (e.timestamp, e.id)
        ORDER BY l.timestamp DESC, l.id DESC
        LIMIT 21
    ) recent
    ORDER BY recent.timestamp ASC, recent.id ASC
) ctx;
"#,
            table = table.quoted()
        ))
        .context("failed to create view error_managing_peer")?;
    Ok(())
}

fn create_indexes(client: &mut Client, table: &SqlTableName) -> Result<()> {
    let index_base = table.index_base();

    client
        .batch_execute(&format!(
            r#"
CREATE INDEX IF NOT EXISTS "{index_base}_timestamp_idx" ON {} (timestamp);
CREATE INDEX IF NOT EXISTS "{index_base}_level_idx" ON {} (level);
CREATE INDEX IF NOT EXISTS "{index_base}_target_idx" ON {} (target);
CREATE INDEX IF NOT EXISTS "{index_base}_torrent_id_idx" ON {} (torrent_id);
CREATE INDEX IF NOT EXISTS "{index_base}_peer_idx" ON {} (peer);
CREATE INDEX IF NOT EXISTS "{index_base}_peer_kind_idx" ON {} (peer_kind);
CREATE INDEX IF NOT EXISTS "{index_base}_raw_gin_idx" ON {} USING GIN (raw);
"#,
            table.quoted(),
            table.quoted(),
            table.quoted(),
            table.quoted(),
            table.quoted(),
            table.quoted(),
            table.quoted()
        ))
        .with_context(|| format!("failed to create indexes on {}", table.quoted()))?;
    Ok(())
}

fn load_log(
    client: &mut Client,
    table: &SqlTableName,
    path: &Path,
    fail_invalid: bool,
) -> Result<Stats> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let copy_sql = format!(
        "COPY {} (timestamp, level, target, message, torrent_id, peer, peer_port, peer_kind, span, spans, fields, raw) \
         FROM STDIN WITH (FORMAT text, NULL '\\N')",
        table.quoted()
    );

    let mut copy = client
        .copy_in(&copy_sql)
        .with_context(|| format!("failed to start COPY into {}", table.quoted()))?;

    let mut stats = Stats {
        loaded: 0,
        skipped: 0,
    };

    for (line_idx, line) in reader.lines().enumerate() {
        let line_no = line_idx as u64 + 1;
        let line = line.with_context(|| format!("failed to read line {line_no}"))?;

        if line.trim().is_empty() {
            stats.skipped += 1;
            continue;
        }

        let row = match parse_log_row(line_no, &line) {
            Ok(row) => row,
            Err(error) if !fail_invalid => {
                eprintln!("skipping {error:#}");
                stats.skipped += 1;
                continue;
            }
            Err(error) => return Err(error),
        };

        write_copy_row(&mut copy, line_no, &row)?;
        stats.loaded += 1;

        if stats.loaded.is_multiple_of(100_000) {
            eprintln!("loaded {} rows", stats.loaded);
        }
    }

    copy.flush().context("failed to flush COPY stream")?;
    copy.finish().context("failed to finish COPY stream")?;
    Ok(stats)
}

fn parse_log_row(line_no: u64, line: &str) -> Result<LogRow> {
    let value: Value =
        serde_json::from_str(line).with_context(|| format!("line {line_no} is not valid JSON"))?;

    let Some(object) = value.as_object() else {
        bail!("line {line_no} is valid JSON but not a JSON object");
    };
    let fields = object
        .get("fields")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let manage_peer = manage_peer_info(object);

    Ok(LogRow {
        timestamp: string_field(object.get("timestamp")),
        level: string_field(object.get("level")),
        target: string_field(object.get("target")),
        message: fields
            .as_object()
            .and_then(|fields| string_field(fields.get("message"))),
        torrent_id: torrent_id(object),
        peer: manage_peer.addr.as_ref().map(|peer| peer.ip().to_string()),
        peer_port: manage_peer.addr.as_ref().map(SocketAddr::port),
        peer_kind: manage_peer.kind,
        span: object.get("span").map(Value::to_string),
        spans: object.get("spans").map(Value::to_string),
        fields: fields.to_string(),
        raw: value.to_string(),
    })
}

fn string_field(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => Some(value.clone()),
        Some(value) => Some(value.to_string()),
        None => None,
    }
}

fn torrent_id(object: &serde_json::Map<String, Value>) -> Option<u16> {
    let span = object.get("span").and_then(Value::as_object);
    if let Some(id) = span.and_then(torrent_id_from_span) {
        return Some(id);
    }

    object
        .get("spans")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_object)
        .find_map(torrent_id_from_span)
}

fn torrent_id_from_span(span: &serde_json::Map<String, Value>) -> Option<u16> {
    if span.get("name").and_then(Value::as_str) != Some("torrent") {
        return None;
    }

    match span.get("id")? {
        Value::Number(id) => id.as_u64()?.try_into().ok(),
        Value::String(id) => id.parse().ok(),
        _ => None,
    }
}

#[derive(Default)]
struct ManagePeerInfo {
    addr: Option<SocketAddr>,
    kind: Option<&'static str>,
}

fn manage_peer_info(object: &serde_json::Map<String, Value>) -> ManagePeerInfo {
    let mut info = ManagePeerInfo::default();

    let span = object.get("span").and_then(Value::as_object);
    if let Some(span) = span {
        if is_manage_peer_span(span) {
            info.addr = manage_peer_addr_from_span(span);
            info.kind = manage_peer_kind_from_span(span);
        }
    }

    let Some(spans) = object.get("spans").and_then(Value::as_array) else {
        return info;
    };

    let mut found_manage_peer = false;
    for span in spans.iter().filter_map(Value::as_object) {
        if is_manage_peer_span(span) {
            found_manage_peer = true;
            info.addr = info.addr.or_else(|| manage_peer_addr_from_span(span));
            info.kind = info.kind.or_else(|| manage_peer_kind_from_span(span));
            continue;
        }

        if found_manage_peer {
            info.kind = info.kind.or_else(|| manage_peer_kind_from_span(span));
        }
    }

    info
}

fn is_manage_peer_span(span: &serde_json::Map<String, Value>) -> bool {
    span.get("name").and_then(Value::as_str) == Some("manage_peer")
}

fn manage_peer_addr_from_span(span: &serde_json::Map<String, Value>) -> Option<SocketAddr> {
    span.get("peer")?.as_str()?.parse().ok()
}

fn manage_peer_kind_from_span(span: &serde_json::Map<String, Value>) -> Option<&'static str> {
    let kind = span
        .get("peer_kind")
        .or_else(|| span.get("kind"))?
        .as_str()?;

    match kind {
        "tcp" => Some("tcp"),
        "utp" | "uTP" => Some("utp"),
        "socks" => Some("socks"),
        _ => None,
    }
}

fn write_copy_row(copy: &mut CopyInWriter, line_no: u64, row: &LogRow) -> Result<()> {
    let fields = [
        row.timestamp.clone(),
        row.level.clone(),
        row.target.clone(),
        row.message.clone(),
        row.torrent_id.map(|id| id.to_string()),
        row.peer.clone(),
        row.peer_port.map(|port| port.to_string()),
        row.peer_kind.map(str::to_owned),
        row.span.clone(),
        row.spans.clone(),
        Some(row.fields.clone()),
        Some(row.raw.clone()),
    ];

    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            copy.write_all(b"\t")
                .with_context(|| format!("failed to write COPY row for line {line_no}"))?;
        }
        write_copy_field(copy, field.as_deref())
            .with_context(|| format!("failed to write COPY row for line {line_no}"))?;
    }
    copy.write_all(b"\n")
        .with_context(|| format!("failed to write COPY row for line {line_no}"))
}

fn write_copy_field<W: Write>(writer: &mut W, value: Option<&str>) -> io::Result<()> {
    let Some(value) = value else {
        return writer.write_all(br"\N");
    };

    for byte in value.bytes() {
        match byte {
            b'\\' => writer.write_all(br"\\")?,
            b'\n' => writer.write_all(br"\n")?,
            b'\r' => writer.write_all(br"\r")?,
            b'\t' => writer.write_all(br"\t")?,
            _ => writer.write_all(&[byte])?,
        }
    }
    Ok(())
}

struct SqlTableName {
    parts: Vec<String>,
}

impl SqlTableName {
    fn parse(input: &str) -> Self {
        Self {
            parts: input.split('.').map(quote_ident).collect(),
        }
    }

    fn quoted(&self) -> String {
        self.parts.join(".")
    }

    fn index_base(&self) -> String {
        self.parts
            .iter()
            .map(|part| {
                part.trim_matches('"')
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("_")
    }
}

fn quote_ident(input: &str) -> String {
    format!(r#""{}""#, input.replace('"', "\"\""))
}
