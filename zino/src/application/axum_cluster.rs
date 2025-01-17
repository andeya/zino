use crate::{endpoint, middleware, AxumExtractor};
use axum::{
    error_handling::HandleErrorLayer,
    extract::{rejection::LengthLimitError, DefaultBodyLimit},
    http::{StatusCode, Uri, uri::PathAndQuery},
    body::Body,
    response::Response as AxumResponse,
    middleware::{from_fn, Next},
    routing, BoxError, Router, Server,
};
use std::{
    convert::Infallible, fs, net::SocketAddr, path::PathBuf, sync::LazyLock, time::Duration, str::FromStr,
};
use tokio::runtime::Builder;
use tower::{
    timeout::{error::Elapsed, TimeoutLayer},
    ServiceBuilder,
};
use tower_cookies::CookieManagerLayer;
use tower_http::{
    compression::{
        predicate::{DefaultPredicate, NotForContentType, Predicate},
        CompressionLayer,
    },
    decompression::DecompressionLayer,
    services::{ServeDir, ServeFile},
};
use utoipa_rapidoc::RapiDoc;
use zino_core::{
    application::Application,
    extension::TomlTableExt,
    response::{FullResponse, Response},
    schedule::{AsyncCronJob, Job, JobScheduler},
};
use crate::{Request, Result};

/// An HTTP server cluster for `axum`.
#[derive(Default)]
pub struct AxumCluster {
    /// Default routes.
    default_routes: Vec<Router>,
    /// Named routes.
    named_routes: Vec<(&'static str, Vec<Router>)>,
}

impl Application for AxumCluster {
    type Routes = Vec<Router>;

    fn register(mut self, routes: Self::Routes) -> Self {
        self.default_routes = routes;
        self
    }

    fn register_with(mut self, server_name: &'static str, routes: Self::Routes) -> Self {
        self.named_routes.push((server_name, routes));
        self
    }

    fn run(self, async_jobs: Vec<(&'static str, AsyncCronJob)>) {
        let runtime = Builder::new_multi_thread()
            .thread_keep_alive(Duration::from_secs(10))
            .thread_stack_size(2 * 1024 * 1024)
            .global_queue_interval(61)
            .enable_all()
            .build()
            .expect("fail to build Tokio runtime for `AxumCluster`");
        let mut scheduler = JobScheduler::new();
        for (cron_expr, exec) in async_jobs {
            scheduler.add(Job::new_async(cron_expr, exec));
        }
        runtime.spawn(async move {
            loop {
                scheduler.tick_async().await;

                // Cannot use `std::thread::sleep` because it blocks the Tokio runtime.
                tokio::time::sleep(scheduler.time_till_next_job()).await;
            }
        });

        runtime.block_on(async {
            let default_routes = self.default_routes;
            let named_routes = self.named_routes;
            let app_state = Self::shared_state();
            let app_name = Self::name();
            let app_version = Self::version();
            let app_env = app_state.env();
            let listeners = app_state.listeners();
            let has_debug_server = listeners.iter().any(|listener| listener.0 == "debug");
            let servers = listeners.into_iter().map(|listener| {
                let server_name = listener.0.as_ref();
                let addr = listener.1;
                tracing::warn!(
                    server_name,
                    app_env,
                    app_name,
                    app_version,
                    "listen on {addr}",
                );

                // Server config
                let project_dir = Self::project_dir();
                let default_public_dir = project_dir.join("public");
                let mut public_route_name = "/public";
                let mut public_dir = PathBuf::new();
                let mut body_limit = 100 * 1024 * 1024; // 100MB
                let mut request_timeout = Duration::from_secs(10); // 10 seconds
                if let Some(config) = app_state.get_config("server") {
                    if let Some(limit) = config.get_usize("body-limit") {
                        body_limit = limit;
                    }
                    if let Some(timeout) = config.get_duration("request-timeout") {
                        request_timeout = timeout;
                    }
                    if let Some(dir) = config.get_str("page-dir") {
                        public_route_name = "/page";
                        public_dir.push(dir);
                    } else if let Some(dir) = config.get_str("public-dir") {
                        public_dir.push(dir);
                    } else {
                        public_dir = default_public_dir;
                    }
                } else {
                    public_dir = default_public_dir;
                }

                // Static pages
                let index_file = public_dir.join("index.html");
                let not_found_file = public_dir.join("404.html");
                let serve_file = ServeFile::new(index_file);
                let mut serve_dir = Router::new()
                    .nest_service(public_route_name, ServeDir::new(public_dir)
                        .precompressed_gzip()
                        .precompressed_br()
                        .append_index_html_on_directories(true)
                        .not_found_service(ServeFile::new(not_found_file)));
                if public_route_name == "/page" {
                    serve_dir = serve_dir.layer(from_fn(
                        // Correct the uri path to achieve the following goals:
                        //   /page -> page-dir/index.html
                        //   /page/login -> page-dir/login.html
                        async move |req: Request, next: Next<Body>| -> Result<AxumResponse> {
                            let (mut head, body) = axum::http::Request::<Body>::from(req).into_parts();
                            let uri_path = head.uri.path();
                            if uri_path == "/page" {
                                let path_query = format!("/page/index.html?{}", head.uri.query().unwrap_or_default());
                                let mut parts = head.uri.into_parts();
                                parts.path_and_query = PathAndQuery::from_str(path_query.as_str()).ok();
                                head.uri = Uri::from_parts(parts).unwrap();
                            } else if let Some((p, name)) = uri_path.rsplit_once("/") {
                                if name.len() > 0 && !name.contains(".") {
                                    let path_query = format!("{p}/{name}.html?{}", head.uri.query().unwrap_or_default());
                                    let mut parts = head.uri.into_parts();
                                    parts.path_and_query = PathAndQuery::from_str(path_query.as_str()).ok();
                                    head.uri = Uri::from_parts(parts).unwrap();
                                }
                            }
                            Ok(next.run(axum::http::request::Request::from_parts(head, body)).await)
                        },
                    ))
                }

                let mut app = Router::new()
                    .route_service("/", serve_file)
                    .merge(serve_dir)
                    .route("/sse", routing::get(endpoint::sse_handler))
                    .route("/websocket", routing::get(endpoint::websocket_handler));
                for route in &default_routes {
                    app = app.merge(route.clone());
                }
                for (name, routes) in &named_routes {
                    if name == &server_name || server_name == "debug" {
                        for route in routes {
                            app = app.merge(route.clone());
                        }
                    }
                }

                // Render OpenAPI docs.
                let docs_server_name = if has_debug_server { "debug" } else { "main" };
                if docs_server_name == server_name {
                    if let Some(config) = app_state.get_config("openapi") {
                        if config.get_bool("show-docs") != Some(false) {
                            // If the `spec-url` has been configured, the user should
                            // provide the generated OpenAPI object with a derivation.
                            let mut rapidoc = if let Some(url) = config.get_str("spec-url") {
                                RapiDoc::new(url)
                            } else {
                                RapiDoc::with_openapi("/api-docs/openapi.json", Self::openapi())
                            };
                            if let Some(route) = config.get_str("rapidoc-route") {
                                rapidoc = rapidoc.path(route);
                            } else {
                                rapidoc = rapidoc.path("/rapidoc");
                            }
                            if let Some(custom_html) = config.get_str("custom-html") &&
                                let Ok(html) = fs::read_to_string(project_dir.join(custom_html))
                            {
                                app = app.merge(rapidoc.custom_html(html.as_str()));
                            } else {
                                app = app.merge(rapidoc);
                            }
                        }
                    } else {
                        let rapidoc =
                            RapiDoc::with_openapi("/api-docs/openapi.json", Self::openapi())
                                .path("/rapidoc");
                        app = app.merge(rapidoc);
                    }
                }

                app = app
                    .fallback_service(tower::service_fn(|req| async {
                        let req = AxumExtractor::from(req);
                        let res = Response::new(StatusCode::NOT_FOUND).context(&req);
                        Ok::<FullResponse, Infallible>(res.into())
                    }))
                    .layer(
                        ServiceBuilder::new()
                            .layer(DefaultBodyLimit::max(body_limit))
                            .layer(CookieManagerLayer::new())
                            .layer(
                                CompressionLayer::new().gzip(true).br(true).compress_when(
                                    DefaultPredicate::new()
                                        .and(NotForContentType::new("application/msgpack")),
                                ),
                            )
                            .layer(DecompressionLayer::new().gzip(true).br(true))
                            .layer(LazyLock::force(&middleware::TRACING_MIDDLEWARE))
                            .layer(LazyLock::force(&middleware::CORS_MIDDLEWARE))
                            .layer(from_fn(middleware::request_context))
                            .layer(from_fn(middleware::etag_middleware))
                            .layer(HandleErrorLayer::new(|err: BoxError| async move {
                                let status_code = if err.is::<Elapsed>() {
                                    StatusCode::REQUEST_TIMEOUT
                                } else if err.is::<LengthLimitError>() {
                                    StatusCode::PAYLOAD_TOO_LARGE
                                } else {
                                    StatusCode::INTERNAL_SERVER_ERROR
                                };
                                let res = Response::new(status_code);
                                Ok::<FullResponse, Infallible>(res.into())
                            }))
                            .layer(TimeoutLayer::new(request_timeout)),
                    );
                Server::bind(&addr).serve(app.into_make_service_with_connect_info::<SocketAddr>())
            });
            for result in futures::future::join_all(servers).await {
                if let Err(err) = result {
                    tracing::error!("server error: {err}");
                }
            }
        });
    }
}
