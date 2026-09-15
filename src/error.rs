//! Anthropic-compatible API errors returned to clients.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub const INVALID_REQUEST: &str = "invalid_request_error";
pub const AUTHENTICATION: &str = "authentication_error";
pub const PERMISSION: &str = "permission_error";
pub const NOT_FOUND: &str = "not_found_error";
pub const RATE_LIMIT: &str = "rate_limit_error";
pub const API: &str = "api_error";
pub const OVERLOADED: &str = "overloaded_error";

/// A terminal API error for one logical request. Never carries upstream
/// content verbatim — messages must be sanitized by the caller.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
    pub request_id: Option<String>,
    /// Suggested retry time for rate-limit errors, epoch seconds.
    pub retry_after_secs: Option<i64>,
}

impl ApiError {
    pub fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            request_id: None,
            retry_after_secs: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, INVALID_REQUEST, message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(401, AUTHENTICATION, message)
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(403, PERMISSION, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, NOT_FOUND, message)
    }

    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self::new(429, RATE_LIMIT, message)
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::new(500, API, message)
    }

    pub fn upstream(status: u16, message: impl Into<String>) -> Self {
        let status = if (500..600).contains(&status) {
            status
        } else {
            502
        };
        Self::new(status, API, message)
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(529, OVERLOADED, message)
    }

    pub fn body(&self) -> serde_json::Value {
        let mut body = json!({
            "type": "error",
            "error": {"type": self.kind, "message": self.message},
        });
        if let Some(rid) = &self.request_id {
            body["request_id"] = json!(rid);
        }
        body
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.kind)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(self.body())).into_response();
        if let Some(secs) = self.retry_after_secs {
            if let Ok(value) = axum::http::HeaderValue::from_str(&secs.max(0).to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
        }
        response
    }
}

impl From<crate::lobsterai::upstream::ApiFailure> for ApiError {
    fn from(failure: crate::lobsterai::upstream::ApiFailure) -> Self {
        let mut error = ApiError::new(failure.status, failure.kind, failure.message);
        error.retry_after_secs = failure.retry_after_secs;
        error
    }
}
