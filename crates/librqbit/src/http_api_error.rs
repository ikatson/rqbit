use std::ops::Deref;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{ser::SerializeMap, Serialize, Serializer};

// Convenience error type.
#[derive(Debug)]
pub struct ApiError {
    status: Option<StatusCode>,
    kind: ApiErrorKind,
}

impl ApiError {
    pub const fn torrent_not_found(torrent_id: usize) -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::TorrentNotFound(torrent_id),
        }
    }
    pub const fn dht_disabled() -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::DhtDisabled,
        }
    }
    pub fn with_status(self, status: StatusCode) -> Self {
        Self {
            status: Some(status),
            kind: self.kind,
        }
    }
}

#[derive(Debug)]
enum ApiErrorKind {
    TorrentNotFound(usize),
    DhtDisabled,
    Other(anyhow::Error),
}

struct ErrWrap<'a, E: ?Sized>(&'a E);

impl<'a, E> Serialize for ErrWrap<'a, E>
where
    E: std::error::Error + ?Sized,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut m = serializer.serialize_map(None)?;
        m.serialize_entry("description", &format!("{}", self.0))?;
        if let Some(source) = self.0.source() {
            m.serialize_entry("source", &ErrWrap(source))?;
        }
        m.end()
    }
}

impl Serialize for ApiErrorKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            ApiErrorKind::TorrentNotFound(id) => {
                let mut m = serializer.serialize_map(None)?;
                m.serialize_entry("error_kind", "torrent_not_found")?;
                m.serialize_entry("id", id)?;
                m.end()
            }
            ApiErrorKind::DhtDisabled => {
                let mut m = serializer.serialize_map(None)?;
                m.serialize_entry("error_kind", "dht_disabled")?;
                m.end()
            }
            ApiErrorKind::Other(err) => {
                let mut m = serializer.serialize_map(None)?;
                m.serialize_entry("error_kind", "internal_error")?;
                m.serialize_entry("human_readable", &format!("{err:#}"))?;
                m.serialize_entry("error_chain", &ErrWrap(err.deref()))?;
                m.end()
            }
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            status: None,
            kind: ApiErrorKind::Other(value),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ApiErrorKind::Other(err) => err.source(),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ApiErrorKind::TorrentNotFound(idx) => write!(f, "torrent {idx} not found"),
            ApiErrorKind::Other(err) => write!(f, "{err:?}"),
            ApiErrorKind::DhtDisabled => write!(f, "DHT is disabled"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = axum::Json(&self.kind).into_response();
        *response.status_mut() = match self.status {
            Some(s) => s,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        response
    }
}

pub trait WithErrorStatus<T> {
    fn with_error_status_code(self, s: StatusCode) -> Result<T, ApiError>;
}

impl<T, E> WithErrorStatus<T> for std::result::Result<T, E>
where
    E: Into<ApiError>,
{
    fn with_error_status_code(self, s: StatusCode) -> Result<T, ApiError> {
        self.map_err(|e| e.into().with_status(s))
    }
}
