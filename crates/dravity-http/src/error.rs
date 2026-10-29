//! API errors and their HTTP status codes.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use dravity_session::SessionError;

use crate::dto::ErrorDto;

/// Why a request failed. Answered as `{"error": "..."}` with a matching
/// status code.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    /// The session refused or failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The request is malformed.
    #[error("{0}")]
    BadRequest(String),
    /// No such torrent or file.
    #[error("{0}")]
    NotFound(String),
}

impl ApiError {
    /// The HTTP status for this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Session(e) => match e {
                SessionError::Unknown(_) | SessionError::NoSuchFile(_) => StatusCode::NOT_FOUND,
                SessionError::Duplicate(_)
                | SessionError::NotRunning(_)
                | SessionError::NoMetadata(_) => StatusCode::CONFLICT,
                SessionError::FileCount { .. }
                | SessionError::MagnetWithoutV1Hash
                | SessionError::StorageForMagnet => StatusCode::BAD_REQUEST,
                SessionError::NoStateDir => StatusCode::NOT_IMPLEMENTED,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::warn!(error = %self, "API request failed");
        }
        (
            status,
            Json(ErrorDto {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}
