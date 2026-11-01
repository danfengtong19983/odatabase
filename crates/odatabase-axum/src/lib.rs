//! Serves an odatabase [`Service`] from an axum router.
//!
//! ```ignore
//! let app = axum::Router::new().nest("/odata", odatabase_axum::router(service));
//! ```
//!
//! Middleware can restrict the request by inserting [`Claims`] into its extensions.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use odatabase_core::{Claims, ODataRequest, Service};

/// Request bodies above this size are rejected.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// A router answering every path below its mount point with `service`.
pub fn router(service: Service) -> Router {
    Router::new()
        .route("/", axum::routing::any(handle))
        .route("/{*path}", axum::routing::any(handle))
        .fallback(handle)
        .with_state(service)
}

async fn handle(
    State(service): State<Service>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    // The original URI still has the nest prefix, which is part of the service root.
    let below_root = path.trim_end_matches('/');
    let prefix = original.path().trim_end_matches('/');
    let prefix = prefix.strip_suffix(below_root).unwrap_or(prefix);
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = parts
        .headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    let root = format!("{scheme}://{host}{prefix}");
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let claims = parts
        .extensions
        .get::<Claims>()
        .cloned()
        .unwrap_or_default();
    let response = service
        .handle(ODataRequest {
            method: parts.method,
            path: path.trim_start_matches('/').to_owned(),
            query: parts.uri.query().unwrap_or("").to_owned(),
            headers: parts.headers,
            body,
            root,
            claims,
        })
        .await;
    (response.status, response.headers, Body::from(response.body)).into_response()
}
