use std::time::Duration;

use serde::Serialize;

use super::{live::stats::snapshot::StatsSnapshot, TorrentStateLive};
use size_format::SizeFormatterBinary as SF;

#[derive(Serialize, Default, Debug)]
pub struct LiveStats {
    pub snapshot: StatsSnapshot,
    pub average_piece_download_time: Option<Duration>,
    pub download_speed: Speed,
    pub upload_speed: Speed,
    pub time_remaining: Option<DurationWithHumanReadable>,
}

impl std::fmt::Display for LiveStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "down speed: {}", self.download_speed)?;
        if let Some(time_remaining) = &self.time_remaining {
            write!(f, ", eta: {time_remaining}")?;
        }
        write!(f, ", up speed: {}", self.upload_speed)?;
        Ok(())
    }
}

impl From<&TorrentStateLive> for LiveStats {
    fn from(live: &TorrentStateLive) -> Self {
        let snapshot = live.stats_snapshot();
        let down_estimator = live.down_speed_estimator();
        let up_estimator = live.up_speed_estimator();

        Self {
            average_piece_download_time: snapshot.average_piece_download_time(),
            snapshot,
            download_speed: down_estimator.mbps().into(),
            upload_speed: up_estimator.mbps().into(),
            time_remaining: down_estimator
                .time_remaining()
                .map(DurationWithHumanReadable),
        }
    }
}

#[derive(Clone, Copy, Serialize, Debug)]
pub enum TorrentStatsState {
    #[serde(rename = "initializing")]
    Initializing,
    #[serde(rename = "live")]
    Live,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "error")]
    Error,
}

impl std::fmt::Display for TorrentStatsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TorrentStatsState::Initializing => f.write_str("initializing"),
            TorrentStatsState::Live => f.write_str("live"),
            TorrentStatsState::Paused => f.write_str("paused"),
            TorrentStatsState::Error => f.write_str("error"),
        }
    }
}

#[derive(Serialize, Debug)]
pub struct TorrentStats {
    pub state: TorrentStatsState,
    pub error: Option<String>,
    pub progress_bytes: u64,
    pub uploaded_bytes: u64,
    pub total_bytes: u64,
    pub finished: bool,
    pub live: Option<LiveStats>,
}

impl std::fmt::Display for TorrentStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", self.state)?;
        if let Some(error) = &self.error {
            return write!(f, "{error}");
        }
        write!(
            f,
            "{} ({})",
            self.progress_percent_human_readable(),
            self.progress_bytes_human_readable()
        )?;
        if let Some(live) = &self.live {
            write!(f, " [{live}]")?;
        }
        Ok(())
    }
}

impl TorrentStats {
    pub fn progress_percent_human_readable(&self) -> impl std::fmt::Display {
        struct Percents {
            progress: u64,
            total: u64,
        }
        impl std::fmt::Display for Percents {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                if self.total == 0 {
                    return write!(f, "N/A");
                }
                let pct = self.progress as f64 / self.total as f64 * 100f64;
                write!(f, "{pct:.2}%")
            }
        }
        Percents {
            progress: self.progress_bytes,
            total: self.total_bytes,
        }
    }

    pub fn progress_bytes_human_readable(&self) -> impl std::fmt::Display {
        struct Progress {
            progress: u64,
            total: u64,
        }
        impl std::fmt::Display for Progress {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{} / {}", SF::new(self.progress), SF::new(self.total))
            }
        }
        Progress {
            progress: self.progress_bytes,
            total: self.total_bytes,
        }
    }
}

fn format_seconds_to_time(seconds: u64, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        write!(f, "{}h {}m", hours, minutes)
    } else if minutes > 0 {
        write!(f, "{}m {}s", minutes, seconds)
    } else {
        write!(f, "{}s", seconds)
    }
}

pub struct DurationWithHumanReadable(Duration);

impl core::fmt::Display for DurationWithHumanReadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> core::fmt::Result {
        format_seconds_to_time(self.0.as_secs(), f)
    }
}

impl core::fmt::Debug for DurationWithHumanReadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl Serialize for DurationWithHumanReadable {
    fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Tmp {
            duration: Duration,
            human_readable: String,
        }
        Tmp {
            duration: self.0,
            human_readable: format!("{}", self),
        }
        .serialize(serializer)
    }
}

#[derive(Default)]
pub struct Speed {
    pub mbps: f64,
}

impl Speed {
    fn new(mbps: f64) -> Self {
        Self { mbps }
    }
}

impl From<f64> for Speed {
    fn from(mbps: f64) -> Self {
        Self::new(mbps)
    }
}

impl core::fmt::Display for Speed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.2} MiB/s", self.mbps)
    }
}

impl core::fmt::Debug for Speed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl Serialize for Speed {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Tmp {
            mbps: f64,
            human_readable: String,
        }
        Tmp {
            mbps: self.mbps,
            human_readable: format!("{}", self),
        }
        .serialize(serializer)
    }
}
