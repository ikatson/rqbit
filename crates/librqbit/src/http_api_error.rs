use std::ops::Deref;

use axum::response::{IntoResponse, Response};
use http::StatusCode;
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

    pub fn status(&self) -> StatusCode {
        self.status.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
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

impl Serialize for ApiError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize, Default)]
        struct SerializedError<'a> {
            error_kind: &'a str,
            human_readable: String,
            status: u16,
            #[serde(skip_serializing_if = "Option::is_none")]
            id: Option<usize>,
            #[serde(skip_serializing_if = "Option::is_none")]
            error_chain: Option<ErrWrap<'a, dyn std::error::Error>>,
        }
        let mut serr: SerializedError = SerializedError {
            error_kind: match self.kind {
                ApiErrorKind::TorrentNotFound(_) => "torrent_not_found",
                ApiErrorKind::DhtDisabled => "dht_disabled",
                ApiErrorKind::Other(_) => "internal_error",
            },
            human_readable: format!("{self}"),
            status: self.status().as_u16(),
            ..Default::default()
        };
        match &self.kind {
            ApiErrorKind::TorrentNotFound(id) => serr.id = Some(*id),
            ApiErrorKind::Other(err) => serr.error_chain = Some(ErrWrap(err.deref())),
            _ => {}
        }
        serr.serialize(serializer)
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
        let mut response = axum::Json(&self).into_response();
        *response.status_mut() = self.status();
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
