use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use odatabase_sql::{Claims, QueryError};
use odatabase_uri::UriError;
use serde_json::json;

/// A request as seen by the service, independent of the web framework.
#[derive(Clone, Debug)]
pub struct ODataRequest {
    /// The HTTP method.
    pub method: Method,
    /// Path below the service root.
    pub path: String,
    /// Raw query string, without the `?`.
    pub query: String,
    /// Request headers.
    pub headers: HeaderMap,
    /// Request body.
    pub body: Bytes,
    /// Absolute URL of the service root, without a trailing slash.
    pub root: String,
    /// What the caller may see and change, set by the application's authentication.
    pub claims: Claims,
}

impl ODataRequest {
    /// A GET request without headers or claims, for tests and internal calls.
    pub fn get(root: &str, path: &str, query: &str) -> Self {
        Self {
            method: Method::GET,
            path: path.to_owned(),
            query: query.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            root: root.to_owned(),
            claims: Claims::default(),
        }
    }
}

/// A response for the adapter to send.
#[derive(Clone, Debug, PartialEq)]
pub struct ODataResponse {
    /// The HTTP status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

impl ODataResponse {
    /// An empty response.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    /// A response with a body of the given content type.
    pub fn with_body(
        status: StatusCode,
        content_type: &'static str,
        body: impl Into<Bytes>,
    ) -> Self {
        let mut r = Self::new(status);
        r.headers
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        r.body = body.into();
        r
    }

    /// A JSON response.
    pub fn json(status: StatusCode, content_type: HeaderValue, value: &serde_json::Value) -> Self {
        let mut r = Self::new(status);
        r.headers.insert(header::CONTENT_TYPE, content_type);
        r.body = serde_json::to_vec(value).unwrap_or_default().into();
        r
    }
}

/// A failed request, reported in the OData JSON error format.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{status}: {message}")]
pub struct ODataError {
    /// The HTTP status.
    pub status: u16,
    /// The message for the client.
    pub message: String,
}

impl ODataError {
    /// An error with any status.
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// A 400 error.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    /// A 404 error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    /// A 501 error.
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(501, message)
    }

    /// The error as an OData JSON error response.
    pub fn to_response(&self) -> ODataResponse {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = json!({"error": {"code": self.status.to_string(), "message": self.message}});
        ODataResponse::json(status, HeaderValue::from_static("application/json"), &body)
    }
}

impl From<UriError> for ODataError {
    fn from(e: UriError) -> Self {
        Self::new(e.status, e.message)
    }
}

impl From<QueryError> for ODataError {
    fn from(e: QueryError) -> Self {
        let (status, message) = e.response();
        // Clients get a generic message, so the cause has to be kept here.
        if status >= 500 {
            tracing::error!(error = %e, "request failed");
        }
        Self::new(status, message)
    }
}
