//! API errors: HTTP status plus `{ "error": "message" }`, and any details
//! (an object) merged in beside it.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Fields that go beside `error` in the body (e.g. the gate's `findings`).
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
    /// Seconds for a `Retry-After` header (rate limits / lockouts).
    pub retry_after: Option<u64>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), details: None, retry_after: None }
    }
    /// With fields beside `error` in the body. Not an object: no details.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = match details {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        };
        self
    }
    /// `Retry-After: <secs>` on the response (typically with 429).
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs.max(1));
        self
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.details.unwrap_or_default();
        body.insert("error".into(), self.message.into());
        let mut res = (self.status, axum::Json(serde_json::Value::Object(body))).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        res
    }
}

/// An internal error: the client gets an opaque message with a reference,
/// the full error chain goes to the log under that reference.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let id = log_ref();
        tracing::error!(log_ref = %id, "{e:#}");
        Self::internal(format!("Something went wrong on the server (ref {id}). The details are in the server log."))
    }
}

/// A short reference for one logged error, e.g. `7Q2M9X4KJD`.
pub fn log_ref() -> String {
    let id = ulid::Ulid::new().to_string();
    id[id.len() - 10..].to_owned()
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        Self::new(e.status(), e.body_text())
    }
}

impl From<op_geo::GeoError> for ApiError {
    fn from(e: op_geo::GeoError) -> Self {
        Self::bad_request(e.to_string())
    }
}

impl From<op_protocol::ProtocolError> for ApiError {
    fn from(e: op_protocol::ProtocolError) -> Self {
        Self::bad_request(e.to_string())
    }
}

/// `axum::Json` whose rejection is an [`ApiError`], so bad bodies get
/// `{ "error": … }` like everything else.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let axum::Json(v) = axum::Json::<T>::from_request(req, state).await?;
        Ok(Self(v))
    }
}
