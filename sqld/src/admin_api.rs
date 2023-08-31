use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::Json;
use futures::TryStreamExt;
use serde::Deserialize;
use std::sync::Arc;
use std::{io::ErrorKind, net::SocketAddr};
use tokio_util::io::ReaderStream;
use url::Url;

use crate::connection::config::{DatabaseConfig, DatabaseConfigStore};
use crate::error::LoadDumpError;
use crate::namespace::{DumpStream, MakeNamespace, NamespaceStore};

struct AppState<F: MakeNamespace> {
    db_config_store: Arc<DatabaseConfigStore>,
    namespaces: Arc<NamespaceStore<F>>,
}

pub async fn run_admin_api<F: MakeNamespace>(
    addr: SocketAddr,
    db_config_store: Arc<DatabaseConfigStore>,
    namespaces: Arc<NamespaceStore<F>>,
) -> anyhow::Result<()> {
    use axum::routing::{get, post};
    let router = axum::Router::new()
        .route("/", get(handle_get_index))
        .route("/v1/config", get(handle_get_config))
        .route("/v1/block", post(handle_post_block))
        .route(
            "/v1/namespaces/:namespace/create",
            post(handle_create_namespace),
        )
        .route("/v1/namespaces/:namespace", delete(handle_delete_namespace))    
        .with_state(Arc::new(AppState {
            db_config_store,
            namespaces,
        }));

    let server = hyper::Server::try_bind(&addr)
        .context("Could not bind admin HTTP API server")?
        .serve(router.into_make_service());

    tracing::info!(
        "Listening for admin HTTP API requests on {}",
        server.local_addr()
    );
    server.await?;
    Ok(())
}

async fn handle_get_index() -> &'static str {
    "Welcome to the sqld admin API"
}

async fn handle_get_config<F: MakeNamespace>(
    State(app_state): State<Arc<AppState<F>>>,
) -> Json<Arc<DatabaseConfig>> {
    Json(app_state.db_config_store.get())
}

#[derive(Debug, Deserialize)]
struct BlockReq {
    block_reads: bool,
    block_writes: bool,
    #[serde(default)]
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateNamespaceReq {
    dump_url: Option<Url>,
}

async fn handle_post_block<F: MakeNamespace>(
    State(app_state): State<Arc<AppState<F>>>,
    Json(req): Json<BlockReq>,
) -> (axum::http::StatusCode, &'static str) {
    let mut config = (*app_state.db_config_store.get()).clone();
    config.block_reads = req.block_reads;
    config.block_writes = req.block_writes;
    config.block_reason = req.block_reason;

    match app_state.db_config_store.store(config) {
        Ok(()) => (axum::http::StatusCode::OK, "OK"),
        Err(err) => {
            tracing::warn!("Could not store database config: {err}");
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Failed")
        }
    }
}

async fn handle_create_namespace<F: MakeNamespace>(
    State(app_state): State<Arc<AppState<F>>>,
    Path(namespace): Path<String>,
    Json(req): Json<CreateNamespaceReq>,
) -> create::Result<()> {
    let maybe_dump = match req.dump_url {
        Some(ref url) => Some(dump_stream_from_url(url).await?),
        None => None,
    };

    app_state
        .namespaces
        .create(namespace.into(), maybe_dump)
        .await?;
    Ok(())
}

async fn dump_stream_from_url(url: &Url) -> Result<DumpStream, LoadDumpError> {
    match url.scheme() {
        "http" => {
            let client = hyper::client::Client::new();
            let uri = url
                .as_str()
                .parse()
                .map_err(|_| LoadDumpError::InvalidDumpUrl)?;
            let resp = client.get(uri).await?;
            let body = resp
                .into_body()
                .map_err(|e| std::io::Error::new(ErrorKind::Other, e));
            Ok(Box::new(body))
        }
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| LoadDumpError::InvalidDumpUrl)?;
            if !path.is_absolute() {
                return Err(LoadDumpError::DumpFilePathNotAbsolute);
            }

            if !path.try_exists()? {
                return Err(LoadDumpError::DumpFileDoesntExist);
            }

            let f = tokio::fs::File::open(path).await?;

            Ok(Box::new(ReaderStream::new(f)))
        }
        scheme => Err(LoadDumpError::UnsupportedUrlScheme(scheme.to_string())),
    }
}

async fn handle_delete_namespace<F: MakeNamespace>(
    State(app_state): State<Arc<AppState<F>>>,
    Path(namespace): Path<String>,
) -> crate::Result<()> {
    todo!();
}
