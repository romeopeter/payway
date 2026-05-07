use axum::{http::HeaderName, Router};
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

const HEADER: &str = "x-request-id";

// Stamps every inbound request with an x-request-id (UUID v4) if it
// doesn't already have one, and copies that ID onto the response so
// upstream callers can correlate. Tracing spans pick it up automatically.
pub fn wrap(router: Router) -> Router {
    let header = HeaderName::from_static(HEADER);
    router.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(header.clone(), MakeRequestUuid))
            .layer(PropagateRequestIdLayer::new(header)),
    )
}
