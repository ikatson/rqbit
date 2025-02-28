use anyhow::Context;
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use futures::future::BoxFuture;
use futures::FutureExt;
use http::{HeaderMap, StatusCode};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, OnFailure};
use tracing::{debug, error_span, info, Span};

use axum::Router;

use crate::api::Api;

use crate::api::Result;
use crate::ApiError;

mod handlers;
mod timeout;
#[cfg(feature = "webui")]
mod webui;

/// An HTTP server for the API.
pub struct HttpApi {
    api: Api,
    opts: HttpApiOptions,
}

#[derive(Debug, Default)]
pub struct HttpApiOptions {
    pub read_only: bool,
    pub basic_auth: Option<(String, String)>,
    #[cfg(feature = "prometheus")]
    pub prometheus_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
}

async fn simple_basic_auth(
    expected_username: Option<&str>,
    expected_password: Option<&str>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<axum::response::Response> {
    let (expected_user, expected_pass) = match (expected_username, expected_password) {
        (Some(u), Some(p)) => (u, p),
        _ => return Ok(next.run(request).await),
    };
    let user_pass = headers
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok());
    let user_pass = match user_pass {
        Some(user_pass) => user_pass,
        None => {
            return Ok((
                StatusCode::UNAUTHORIZED,
                [("WWW-Authenticate", "Basic realm=\"API\"")],
            )
                .into_response())
        }
    };
    // TODO: constant time compare
    match user_pass.split_once(':') {
        Some((u, p)) if u == expected_user && p == expected_pass => Ok(next.run(request).await),
        _ => Err(ApiError::unathorized()),
    }
}

impl HttpApi {
    pub fn new(api: Api, opts: Option<HttpApiOptions>) -> Self {
        Self {
            api,
            opts: opts.unwrap_or_default(),
        }
    }

    /// Run the HTTP server forever on the given address.
    /// If read_only is passed, no state-modifying methods will be exposed.
    #[inline(never)]
    pub fn make_http_api_and_run(
        mut self,
        listener: TcpListener,
        upnp_router: Option<Router>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        #[cfg(feature = "prometheus")]
        let mut prometheus_handle = self.opts.prometheus_handle.take();

        let state = Arc::new(self);

        let mut main_router = handlers::make_api_router(state.clone());

        #[cfg(feature = "webui")]
        {
            use axum::response::Redirect;

            let webui_router = webui::make_webui_router();
            main_router = main_router.nest("/web/", webui_router);
            main_router = main_router.route("/web", get(|| async { Redirect::permanent("./web/") }))
        }

        #[cfg(feature = "prometheus")]
        if let Some(handle) = prometheus_handle.take() {
            main_router =
                main_router.route("/metrics", get(move || async move { handle.render() }));
        }

        let cors_layer = {
            use tower_http::cors::{AllowHeaders, AllowOrigin};

            const ALLOWED_ORIGINS: [&[u8]; 4] = [
                // Webui-dev
                b"http://localhost:3031",
                b"http://127.0.0.1:3031",
                // Tauri dev
                b"http://localhost:1420",
                // Tauri prod
                b"tauri://localhost",
            ];

            let allow_regex = std::env::var("CORS_ALLOW_REGEXP")
                .ok()
                .and_then(|value| regex::bytes::Regex::new(&value).ok());

            tower_http::cors::CorsLayer::default()
                .allow_origin(AllowOrigin::predicate(move |v, _| {
                    ALLOWED_ORIGINS.contains(&v.as_bytes())
                        || allow_regex
                            .as_ref()
                            .map(move |r| r.is_match(v.as_bytes()))
                            .unwrap_or(false)
                }))
                .allow_headers(AllowHeaders::any())
        };

        // Simple one-user basic auth
        if let Some((user, pass)) = state.opts.basic_auth.clone() {
            info!("Enabling simple basic authentication in HTTP API");
            main_router = main_router.route_layer(axum::middleware::from_fn(
                move |headers, request, next| {
                    let user = user.clone();
                    let pass = pass.clone();
                    async move {
                        simple_basic_auth(Some(&user), Some(&pass), headers, request, next).await
                    }
                },
            ));
        }

        if let Some(upnp_router) = upnp_router {
            main_router = main_router.nest("/upnp", upnp_router);
        }

        let app = main_router
            .layer(cors_layer)
            .layer(
                tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(|req: &Request| {
                        let method = req.method();
                        let uri = req.uri();
                        if let Some(ConnectInfo(addr)) =
                            req.extensions().get::<ConnectInfo<SocketAddr>>()
                        {
                            let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
                            error_span!("request", %method, %uri, %addr)
                        } else {
                            error_span!("request", %method, %uri)
                        }
                    })
                    .on_request(|req: &Request, _: &Span| {
                        if req.uri().path().starts_with("/upnp") {
                            debug!(headers=?req.headers())
                        }
                    })
                    .on_response(DefaultOnResponse::new().include_headers(true))
                    .on_failure({
                        let mut default = DefaultOnFailure::new();
                        move |failure_class, latency, span: &Span| match failure_class {
                            tower_http::classify::ServerErrorsFailureClass::StatusCode(
                                StatusCode::NOT_IMPLEMENTED,
                            ) => {}
                            _ => default.on_failure(failure_class, latency, span),
                        }
                    }),
            )
            .into_make_service_with_connect_info::<SocketAddr>();

        async move {
            axum::serve(listener, app)
                .await
                .context("error running HTTP API")
        }
        .boxed()
    }
}
