// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

mod actor;
mod auth;
mod authoring;
mod contexts;
mod failure;
mod maintenance;
mod mcp;
mod templates;
mod writes;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use clap::Parser;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use serde_json::json;

#[derive(Parser)]
#[command(group(clap::ArgGroup::new("offline_action").args(["preview_migration", "build_context_fixture"]).multiple(false)))]
struct Args {
    #[arg(long, conflicts_with = "offline_action")]
    run_maintenance: bool,
    #[arg(long, requires = "collection")]
    preview_migration: Option<i64>,
    #[arg(long, requires = "collection")]
    build_context_fixture: Option<std::path::PathBuf>,
    #[arg(long, requires = "offline_action")]
    collection: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.run_maintenance {
        return actor::run_maintenance().await;
    }
    if let Some(path) = args.collection {
        anyhow::ensure!(
            path.exists(),
            "offline inspection requires an existing disposable copy"
        );
        let mut col = anki::collection::CollectionBuilder::new(path).build()?;
        if let Some(id) = args.preview_migration {
            println!("{}", contexts::migration_preview(&mut col, id)?);
        } else if let Some(output) = args.build_context_fixture {
            println!("{}", contexts::build_fixture(&mut col, &output)?);
        }
        col.close(None)?;
        return Ok(());
    }
    tracing_subscriber::fmt().with_target(false).init();
    let issuer = std::env::var("ANKI_OIDC_ISSUER")?;
    let resource = std::env::var("ANKI_MCP_RESOURCE")?;
    let transport_config = transport_config(&resource)?;
    let owner = std::env::var("ANKI_OWNER_SUBJECT")?;
    let auth = auth::Auth::new(issuer, resource, owner).await?;
    let actor = actor::Actor::start()?;
    let factory = actor.clone();
    let service: StreamableHttpService<mcp::Mcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(mcp::Mcp::new(factory.clone())),
        Arc::default(),
        transport_config,
    );
    let protected =
        Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(
                auth.clone(),
                auth::authenticate,
            ));
    let metadata_auth = auth.clone();
    let metadata = move || {
        let auth = metadata_auth.clone();
        async move {
            Json(
                json!({"resource": auth.resource, "authorization_servers": [auth.issuer],
            "scopes_supported": ["anki:read", "anki:sync", "anki:write_notes", "anki:maintenance"], "bearer_methods_supported": ["header"]}),
            )
        }
    };
    let ready = actor.ready.clone();
    let router = Router::new()
        .merge(protected)
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(metadata.clone()),
        )
        .route("/.well-known/oauth-protected-resource", get(metadata))
        .route(
            "/healthz",
            get(move || {
                let ready = ready.clone();
                async move {
                    if ready.load(Ordering::Acquire) {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        })
        .await?;
    Ok(())
}

fn transport_config(resource: &str) -> Result<StreamableHttpServerConfig> {
    let resource_url = reqwest::Url::parse(resource)?;
    let public_host = resource_url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("MCP resource must have a hostname"))?;
    let public_authority = match resource_url.port() {
        Some(port) => format!("{public_host}:{port}"),
        None => public_host.to_string(),
    };
    Ok(StreamableHttpServerConfig::default()
        .with_allowed_hosts([public_authority])
        .with_json_response(true)
        .with_max_request_body_bytes(128 * 1024))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use rmcp::model::ServerCapabilities;
    use rmcp::model::ServerConfig;
    use rmcp::ServerHandler;

    use super::*;

    #[derive(Clone)]
    struct Discovery;

    impl ServerHandler for Discovery {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().build())
        }
    }

    #[tokio::test]
    async fn discovery_accepts_public_host_and_rejects_unrelated_hosts() {
        let service: StreamableHttpService<Discovery, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(Discovery),
                Arc::default(),
                transport_config("https://mcp.jupiter.timdumol.com/mcp").unwrap(),
            );
        for (host, expected) in [
            ("mcp.jupiter.timdumol.com", StatusCode::OK),
            ("attacker.example", StatusCode::FORBIDDEN),
            ("localhost", StatusCode::FORBIDDEN),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("Host", host)
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                            "clientInfo": {"name": "discovery-test", "version": "1"}}})
                    .to_string(),
                ))
                .unwrap();
            assert_eq!(service.handle(request).await.status(), expected, "{host}");
        }
    }
}
