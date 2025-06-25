#[cfg(feature = "http-api")]
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::{Serialize, Serializer};

use crate::api::TorrentIdOrHash;

// Convenience error type.
#[derive(Debug)]
pub struct ApiError {
    status: Option<StatusCode>,
    kind: ApiErrorKind,
    plaintext: bool,
}

impl ApiError {
    pub fn new_from_anyhow(status: StatusCode, error: anyhow::Error) -> Self {
        Self {
            status: Some(status),
            kind: ApiErrorKind::OtherAnyhow(error),
            plaintext: false,
        }
    }

    pub fn new_from_error(status: StatusCode, error: crate::Error) -> Self {
        Self {
            status: Some(status),
            kind: ApiErrorKind::OtherError(error),
            plaintext: false,
        }
    }

    pub const fn torrent_not_found(torrent_id: TorrentIdOrHash) -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::TorrentNotFound(torrent_id),
            plaintext: false,
        }
    }

    pub const fn new_from_text(status: StatusCode, text: &'static str) -> Self {
        Self {
            status: Some(status),
            kind: ApiErrorKind::Text(text),
            plaintext: false,
        }
    }

    #[allow(dead_code)]
    pub fn not_implemented(msg: &'static str) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
            kind: ApiErrorKind::Text(msg),
            plaintext: false,
        }
    }

    pub const fn dht_disabled() -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::DhtDisabled,
            plaintext: false,
        }
    }

    pub const fn unathorized() -> Self {
        Self {
            status: Some(StatusCode::UNAUTHORIZED),
            kind: ApiErrorKind::Unauthorized,
            plaintext: true,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    pub fn with_status(self, status: StatusCode) -> Self {
        Self {
            status: Some(status),
            kind: self.kind,
            plaintext: self.plaintext,
        }
    }

    pub fn with_plaintext_error(self, value: bool) -> Self {
        Self {
            status: self.status,
            kind: self.kind,
            plaintext: value,
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum ApiErrorKind {
    #[error("torrent not found {0}")]
    TorrentNotFound(TorrentIdOrHash),
    #[error("DHT is disabled")]
    DhtDisabled,
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Text(&'static str),
    #[error(transparent)]
    OtherAnyhow(#[from] anyhow::Error),
    #[error(transparent)]
    OtherError(#[from] crate::Error),
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
            status_text: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            id: Option<TorrentIdOrHash>,
        }
        let mut serr: SerializedError = SerializedError {
            error_kind: match self.kind {
                ApiErrorKind::TorrentNotFound(_) => "torrent_not_found",
                ApiErrorKind::DhtDisabled => "dht_disabled",
                ApiErrorKind::Unauthorized => "unathorized",
                ApiErrorKind::OtherAnyhow(_) => "internal_error",
                ApiErrorKind::OtherError(_) => "internal_error",
                ApiErrorKind::Text(_) => "internal_error",
            },
            human_readable: format!("{self}"),
            status: self.status().as_u16(),
            status_text: self.status().to_string(),
            ..Default::default()
        };
        if let ApiErrorKind::TorrentNotFound(id) = &self.kind {
            serr.id = Some(*id)
        }
        serr.serialize(serializer)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        let status = value.downcast_ref::<ApiError>().and_then(|e| e.status);
        Self {
            status,
            kind: ApiErrorKind::OtherAnyhow(value),
            plaintext: false,
        }
    }
}

impl From<crate::Error> for ApiError {
    fn from(e: crate::Error) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
            kind: ApiErrorKind::OtherError(e),
            plaintext: false,
        }
    }
}

impl<E> From<(StatusCode, E)> for ApiError
where
    ApiErrorKind: From<E>,
{
    fn from(value: (StatusCode, E)) -> Self {
        Self {
            status: Some(value.0),
            kind: value.1.into(),
            plaintext: false,
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ApiErrorKind::OtherAnyhow(err) => Some(err.as_ref()),
            ApiErrorKind::OtherError(err) => Some(err),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.kind)
    }
}

#[cfg(feature = "http-api")]
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = axum::Json(&self).into_response();
        *response.status_mut() = self.status();
        response
    }
}

pub trait ApiErrorExt<T> {
    fn with_error_status_code(self, s: StatusCode) -> Result<T, ApiError>;
    #[allow(dead_code)]
    fn with_plaintext_error(self, value: bool) -> Result<T, ApiError>;
}

impl<T, E> ApiErrorExt<T> for std::result::Result<T, E>
where
    E: Into<ApiError>,
{
    fn with_error_status_code(self, s: StatusCode) -> Result<T, ApiError> {
        self.map_err(|e| e.into().with_status(s))
    }

    fn with_plaintext_error(self, value: bool) -> Result<T, ApiError> {
        self.map_err(|e| e.into().with_plaintext_error(value))
    }
}
