use std::io::LineWriter;

use anyhow::Context;
use bytes::Bytes;
use librqbit_core::spawn_utils::spawn;
use tracing::error_span;
use tracing_subscriber::Layer;
use tracing_subscriber::{
    fmt::{self, MakeWriter},
    prelude::*,
    EnvFilter,
};

struct Subscriber {
    tx: tokio::sync::broadcast::Sender<Bytes>,
}

struct Writer {
    tx: tokio::sync::broadcast::Sender<Bytes>,
}

pub type LineBroadcast = tokio::sync::broadcast::Sender<Bytes>;

impl Subscriber {
    pub fn new() -> (Self, LineBroadcast) {
        let (tx, _) = tokio::sync::broadcast::channel(100);
        (Self { tx: tx.clone() }, tx)
    }
}

impl<'a> MakeWriter<'a> for Subscriber {
    type Writer = LineWriter<Writer>;

    fn make_writer(&self) -> Self::Writer {
        LineWriter::new(Writer {
            tx: self.tx.clone(),
        })
    }
}

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = buf.len();
        if self.tx.receiver_count() == 0 {
            return Ok(len);
        }
        let arc = buf.to_vec().into();
        let _ = self.tx.send(arc);
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct InitLoggingOptions<'a> {
    pub default_rust_log_value: Option<&'a str>,
    pub log_file: Option<&'a str>,
    pub log_file_rust_log: Option<&'a str>,
    pub log_file_json: bool,
    pub log_json: bool,
}

pub struct InitLoggingResult {
    pub rust_log_reload_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pub line_broadcast: LineBroadcast,
}

#[inline(never)]
pub fn init_logging(opts: InitLoggingOptions) -> anyhow::Result<InitLoggingResult> {
    let initial_filter = EnvFilter::builder()
        .with_default_directive(
            opts.default_rust_log_value
                .unwrap_or("info")
                .parse()
                .context("can't parse provided rust_log value")?,
        )
        .from_env()
        .context("invalid RUST_LOG value")?;

    let stdout_layer: Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync> = if opts.log_json
    {
        Box::new(fmt::layer().json())
    } else {
        Box::new(fmt::layer())
    };

    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(initial_filter);

    let (line_sub, line_broadcast) = Subscriber::new();

    let layered = tracing_subscriber::registry()
        .with(stdout_layer.with_filter(filter_layer))
        // HTTP API log broadcast layer.
        .with(
            fmt::layer()
                .with_ansi(false)
                .fmt_fields(tracing_subscriber::fmt::format::JsonFields::new())
                .event_format(fmt::format().with_ansi(false).json())
                .with_writer(line_sub)
                .with_filter(EnvFilter::builder().parse("info,librqbit=debug").unwrap()),
        );
    #[cfg(feature = "tokio-console")]
    let console_layer = console_subscriber::spawn();

    #[cfg(feature = "tokio-console")]
    let layered = layered.with(console_layer);

    if let Some(log_file) = &opts.log_file {
        let log_file = log_file.to_string();
        let log_file = std::sync::Mutex::new(LineWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_file)
                .with_context(|| format!("error opening log file {:?}", log_file))?,
        ));
        let log_env_filter = EnvFilter::builder()
            .parse(opts.log_file_rust_log.unwrap_or("info,librqbit=debug"))
            .context("can't parse log-file-rust-log")?;
        if opts.log_file_json {
            layered
                .with(
                    fmt::layer()
                        .json()
                        .with_writer(log_file)
                        .with_filter(log_env_filter),
                )
                .try_init()
                .context("can't init json file logging")?;
        } else {
            layered
                .with(
                    fmt::layer()
                        .with_ansi(false)
                        .with_writer(log_file)
                        .with_filter(log_env_filter),
                )
                .try_init()
                .context("can't init logging to file")?;
        }
    } else {
        layered.try_init().context("can't init logging")?;
    }

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    spawn(error_span!("fmt_filter_reloader"), async move {
        while let Some(rust_log) = reload_rx.recv().await {
            let stdout_env_filter = match EnvFilter::builder().parse(&rust_log) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("can't parse env filter {:?}: {:#?}", rust_log, e);
                    continue;
                }
            };
            eprintln!("setting RUST_LOG to {:?}", rust_log);
            let _ = reload_handle.reload(stdout_env_filter);
        }
        Ok(())
    });
    Ok(InitLoggingResult {
        rust_log_reload_tx: reload_tx,
        line_broadcast,
    })
}
