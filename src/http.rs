// axum on tokio: the pairing pages, and rmcp's streamable http service for paired apps

use crate::{auth, mcp};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use std::net::TcpListener;
use std::sync::Arc;

const LOGO: &[u8] = include_bytes!("../res/logo.png");

// a restart starts the new process before the old one closes the port
pub fn listen(port: u16) -> Option<TcpListener> {
    (0..20).find_map(|_| {
        TcpListener::bind(("0.0.0.0", port)).ok().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(250));
            None
        })
    })
}

pub fn serve(listener: TcpListener) {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        listener.set_nonblocking(true).unwrap();
        // apps reach this computer by its lan address, so host checks only the dns rebinding case
        let config = StreamableHttpServerConfig::default().disable_allowed_hosts().with_json_response(true);
        let mcp = StreamableHttpService::new(|| Ok(mcp::Seven), Arc::new(LocalSessionManager::default()), config);
        let app = Router::new()
            .route_service("/mcp", mcp)
            .route_layer(middleware::from_fn(auth::check))
            .route("/", get(auth::start).post(auth::finish))
            .route("/logo.png", get(|| async { ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "max-age=86400")], LOGO) }))
            .layer(middleware::from_fn(host));
        axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap();
    });
}

pub fn base(headers: &HeaderMap) -> String {
    format!("http://{}", headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost"))
}

// dns rebinding needs a domain name, so allow only an ip address or this computer's name
async fn host(req: Request, next: Next) -> Response {
    let base = base(req.headers());
    let host = base.trim_start_matches("http://");
    let name = match host.strip_prefix('[') {
        Some(ipv6) => ipv6.split(']').next(),
        None => host.split(':').next(),
    }
    .unwrap_or("");
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
    if name.parse::<std::net::IpAddr>().is_ok() || name.eq_ignore_ascii_case("localhost") || name.eq_ignore_ascii_case(&computer) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}
