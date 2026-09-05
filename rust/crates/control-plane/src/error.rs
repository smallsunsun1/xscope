use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sea_orm::DbErr;
use serde_json::json;
use thiserror::Error;
use xscope_domain::DomainError;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{0}")]
    Invalid(String),
    #[error("resource not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    InsufficientFunds(String),
    #[error("authentication is required")]
    Unauthorized,
    #[error("access is forbidden")]
    Forbidden,
    #[error("internal authentication is unavailable")]
    InternalAuthUnavailable,
    #[error("invalid internal token")]
    InvalidInternalToken,
    #[error("dependent service is unavailable: {0}")]
    Dependency(String),
    #[error("database operation failed")]
    Database(#[from] DbErr),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<DomainError> for ServiceError {
    fn from(value: DomainError) -> Self {
        Self::Invalid(value.to_string())
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let (status, code, public_message) = match &self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", message.clone()),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "resource not found".to_owned(),
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message.clone()),
            Self::InsufficientFunds(message) => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_limit_exceeded",
                message.clone(),
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_required",
                "console authentication is required".to_owned(),
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "the current user cannot access this tenant".to_owned(),
            ),
            Self::InternalAuthUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "internal_auth_unavailable",
                "internal authentication is not configured".to_owned(),
            ),
            Self::InvalidInternalToken => (
                StatusCode::UNAUTHORIZED,
                "invalid_internal_token",
                "a valid internal token is required".to_owned(),
            ),
            Self::Dependency(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                self.to_string(),
            ),
            Self::Database(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "platform database is unavailable".to_owned(),
            ),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "an internal error occurred".to_owned(),
            ),
        };
        if matches!(self, Self::Database(_) | Self::Internal(_)) {
            tracing::error!(error = %self, "request failed");
        }
        (
            status,
            Json(json!({"error": {"code": code, "message": public_message}})),
        )
            .into_response()
    }
}

pub type ServiceResult<T> = Result<T, ServiceError>;
