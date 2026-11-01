use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use odatabase_sql::{Claims, QueryError};
use odatabase_uri::UriError;
use serde_json::json;

/// A request as seen by the service, independent of the web framework.
#[derive(Clone, Debug)]
pub struct ODataRequest {
    pub method: Method,
    /// Path below the service root.
    pub path: String,
    /// Raw query string, without the `?`.
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// Absolute URL of the service root, without a trailing slash.
    pub root: String,
    pub claims: Claims,
}

impl ODataRequest {
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

#[derive(Clone, Debug, PartialEq)]
pub struct ODataResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl ODataResponse {
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

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
    pub status: u16,
    pub message: String,
}

impl ODataError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(501, message)
    }

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
        Self::new(status, message)
    }
}
