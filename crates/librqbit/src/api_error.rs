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
}

pub trait WithStatus<T> {
    fn with_status(self, status: StatusCode) -> Result<T, ApiError>;
}

pub trait WithStatusError<T> {
    fn with_status_error<E: Into<ApiErrorKind>>(
        self,
        status: StatusCode,
        err: E,
    ) -> Result<T, ApiError>;
}

impl<T> WithStatusError<T> for Option<T> {
    fn with_status_error<E: Into<ApiErrorKind>>(
        self,
        status: StatusCode,
        err: E,
    ) -> Result<T, ApiError> {
        self.ok_or(ApiError {
            status: Some(status),
            kind: err.into(),
        })
    }
}

impl<T, RE> WithStatusError<T> for Result<T, RE> {
    fn with_status_error<E: Into<ApiErrorKind>>(
        self,
        status: StatusCode,
        err: E,
    ) -> Result<T, ApiError> {
        self.map_err(|_| ApiError::from((status, err.into())))
    }
}

impl<T, RE> WithStatus<T> for Result<T, RE>
where
    ApiErrorKind: From<RE>,
{
    fn with_status(self, status: StatusCode) -> Result<T, ApiError> {
        self.map_err(|e| ApiError::from((status, ApiErrorKind::from(e))))
    }
}

impl ApiError {
    pub const fn torrent_not_found(torrent_id: TorrentIdOrHash) -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::TorrentNotFound(torrent_id),
        }
    }

    #[allow(dead_code)]
    pub fn not_implemented(msg: &'static str) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
            kind: ApiErrorKind::Text(msg),
        }
    }

    pub const fn dht_disabled() -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::DhtDisabled,
        }
    }

    pub const fn unathorized() -> Self {
        Self {
            status: Some(StatusCode::UNAUTHORIZED),
            kind: ApiErrorKind::Unauthorized,
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ApiErrorKind {
    #[error("torrent not found {0}")]
    TorrentNotFound(TorrentIdOrHash),
    #[error("DHT is disabled")]
    DhtDisabled,
    #[error("unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Text(&'static str),

    // TODO: consider just boxing all other errors into anyhow
    #[error(transparent)]
    OtherAnyhow(#[from] anyhow::Error),
    #[error(transparent)]
    OtherCore(#[from] librqbit_core::Error),
    #[error(transparent)]
    OtherError(#[from] crate::Error),
}

impl From<&'static str> for ApiErrorKind {
    fn from(value: &'static str) -> Self {
        Self::Text(value)
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
                ApiErrorKind::OtherCore(_) => "internal_error",
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
        }
    }
}

impl From<crate::Error> for ApiError {
    fn from(e: crate::Error) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
            kind: ApiErrorKind::OtherError(e),
        }
    }
}

impl From<librqbit_core::Error> for ApiError {
    fn from(e: librqbit_core::Error) -> Self {
        Self {
            status: Some(StatusCode::INTERNAL_SERVER_ERROR),
            kind: ApiErrorKind::OtherCore(e),
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
