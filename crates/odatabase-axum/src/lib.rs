//! Serves an odatabase [`Service`] from an axum router.
//!
//! ```ignore
//! let app = odatabase_axum::mount("/odata", service);
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

#[derive(Clone)]
struct Mounted {
    service: Service,
    prefix: String,
}

/// A router serving `service` at `prefix`. Merge it into your application's router.
///
/// The root answers with and without a trailing slash, which `Router::nest` cannot do,
/// and OData clients commonly add one.
pub fn mount(prefix: &str, service: Service) -> Router {
    let prefix = prefix.trim_end_matches('/').to_owned();
    let handler = axum::routing::any(handle);
    let mut router = Router::new()
        .route(&format!("{prefix}/"), handler.clone())
        .route(&format!("{prefix}/{{*path}}"), handler.clone());
    if !prefix.is_empty() {
        router = router.route(&prefix, handler);
    }
    router.with_state(Mounted { service, prefix })
}

async fn handle(
    State(Mounted { service, prefix }): State<Mounted>,
    OriginalUri(original): OriginalUri,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path()
        .strip_prefix(prefix.as_str())
        .unwrap_or("")
        .to_owned();
    // The original URI includes any outer `nest`, which is part of the service root.
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
