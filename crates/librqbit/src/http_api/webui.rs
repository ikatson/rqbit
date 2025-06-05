use axum::{Router, routing::get};

pub fn make_webui_router() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [("Content-Type", "text/html")],
                    include_str!("../../webui/dist/index.html"),
                )
            }),
        )
        .route(
            "/assets/index.js",
            get(|| async {
                (
                    [("Content-Type", "application/javascript")],
                    include_str!("../../webui/dist/assets/index.js"),
                )
            }),
        )
        .route(
            "/assets/index.css",
            get(|| async {
                (
                    [("Content-Type", "text/css")],
                    include_str!("../../webui/dist/assets/index.css"),
                )
            }),
        )
        .route(
            "/assets/logo.svg",
            get(|| async {
                (
                    [("Content-Type", "image/svg+xml")],
                    include_str!("../../webui/dist/assets/logo.svg"),
                )
            }),
        )
}
