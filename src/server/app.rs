//! The dashboard app: the React page built in `web/` (`npm run build`), embedded in the program so there is nothing separate to host.
//! The build is committed, so `cargo install` works without Node. It is one page, one script and one stylesheet with fixed names; any
//! address that is not an API or sign-in route returns the page, and the page's own router shows the right screen.

use super::web::secure;
use axum::{
    http::{HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};

const INDEX: &str = include_str!("../../web/dist/index.html");
const SCRIPT: &str = include_str!("../../web/dist/app.js");
const STYLE: &str = include_str!("../../web/dist/app.css");

/// A response with the app's content policy: scripts and styles only from this origin, images from it or inline `data:`.
fn app_response(body: &'static str, content_type: &'static str, cache: &'static str) -> Response {
    let mut r = secure(body.into_response(), content_type, cache);
    r.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    );
    r
}

/// The page's script.
pub(crate) async fn script() -> Response {
    app_response(SCRIPT, "text/javascript; charset=utf-8", "no-cache")
}

/// The page's stylesheet.
pub(crate) async fn style() -> Response {
    app_response(STYLE, "text/css; charset=utf-8", "no-cache")
}

/// Everything else: the page itself, except that a mistyped API or sign-in address is a plain not-found, not a page.
pub(crate) async fn fallback(uri: Uri) -> Response {
    let p = uri.path();
    if p.starts_with("/api/") || p.starts_with("/auth/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    app_response(INDEX, "text/html; charset=utf-8", "no-cache")
}
