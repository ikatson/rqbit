# rqbit-log-to-postgres

Load `tracing_subscriber::fmt::json()` newline-delimited logs into PostgreSQL.

The loader streams the input file line-by-line and writes rows with PostgreSQL
`COPY FROM STDIN`, so it is suitable for very large log files.

```bash
cd scripts/rqbit-log-to-postgres
cargo run --release -- --file /tmp/rqbit-log
```

By default it connects to `postgresql:///rqbit-log`, drops and
recreates a table named `rqbit_logs`, then loads these columns:

- `timestamp`
- `level`
- `target`
- `message`
- `torrent_id`
- `peer`
- `peer_port`
- `peer_kind`
- `span`
- `spans`
- `fields`
- `raw`

Useful options:

```bash
# Use a custom destination table.
cargo run --release -- --file /tmp/rqbit-log --table public.rqbit_logs

# Fail on malformed JSON lines instead of skipping them.
cargo run --release -- --file /tmp/rqbit-log --fail-invalid

# Create query indexes after loading.
cargo run --release -- --file /tmp/rqbit-log --create-indexes
```

Use `--database-url` or `DATABASE_URL` to override the default connection string.

The loader also creates `error_managing_peer`, a view containing each
`manage_peer finished with error` row and up to 20 preceding messages for the
same `torrent_id`, `peer`, and `peer_port`.

```sql
SELECT *
FROM error_managing_peer
WHERE error_message !~ '(?i)(timed out|connection reset|connection refused)'
ORDER BY error_at DESC, error_id DESC, msg_at ASC, msg_id ASC;
```
