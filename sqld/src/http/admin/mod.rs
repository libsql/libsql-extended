use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::routing::delete;
use axum::Json;
use chrono::NaiveDateTime;
use futures::TryStreamExt;
use hyper::Body;
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::io::ReaderStream;
use url::Url;

use crate::database::Database;
use crate::error::LoadDumpError;
use crate::hrana;
use crate::namespace::{DumpStream, MakeNamespace, NamespaceName, NamespaceStore, RestoreOption};
use crate::net::Connector;
use crate::LIBSQL_PAGE_SIZE;

pub mod stats;

type UserHttpServer<M> =
    Arc<hrana::http::Server<<<M as MakeNamespace>::Database as Database>::Connection>>;

struct AppState<M: MakeNamespace, C> {
    namespaces: NamespaceStore<M>,
    user_http_server: UserHttpServer<M>,
    connector: C,
}

pub async fn run<M, A, C>(
    acceptor: A,
    user_http_server: UserHttpServer<M>,
    namespaces: NamespaceStore<M>,
    connector: C,
) -> anyhow::Result<()>
where
    A: crate::net::Accept,
    M: MakeNamespace,
    C: Connector,
{
    use axum::routing::{get, post};
    let router = axum::Router::new()
        .route("/", get(handle_get_index))
        .route(
            "/v1/namespaces/:namespace/config",
            get(handle_get_config).post(handle_post_config),
        )
        .route(
            "/v1/namespaces/:namespace/fork/:to",
            post(handle_fork_namespace),
        )
        .route(
            "/v1/namespaces/:namespace/create",
            post(handle_create_namespace),
        )
        .route("/v1/namespaces/:namespace", delete(handle_delete_namespace))
        .route("/v1/namespaces/:namespace/stats", get(stats::handle_stats))
        .route("/v1/diagnostics", get(handle_diagnostics))
        .with_state(Arc::new(AppState {
            namespaces,
            connector,
            user_http_server,
        }));

    hyper::server::Server::builder(acceptor)
        .serve(router.into_make_service())
        .await
        .context("Could not bind admin HTTP API server")?;
    Ok(())
}

async fn handle_get_index() -> &'static str {
    "Welcome to the sqld admin API"
}

async fn handle_get_config<M: MakeNamespace, C: Connector>(
    State(app_state): State<Arc<AppState<M, C>>>,
    Path(namespace): Path<String>,
) -> crate::Result<Json<HttpDatabaseConfig>> {
    let store = app_state
        .namespaces
        .config_store(NamespaceName::from_string(namespace)?)
        .await?;
    let config = store.get();
    let max_db_size = bytesize::ByteSize::b(config.max_db_pages * LIBSQL_PAGE_SIZE);
    let resp = HttpDatabaseConfig {
        block_reads: config.block_reads,
        block_writes: config.block_writes,
        block_reason: config.block_reason.clone(),
        max_db_size: Some(max_db_size),
    };

    Ok(Json(resp))
}

async fn handle_diagnostics<M: MakeNamespace, C>(
    State(app_state): State<Arc<AppState<M, C>>>,
) -> crate::Result<Json<Vec<String>>> {
    use crate::connection::Connection;
    use hrana::http::stream;

    let server = app_state.user_http_server.as_ref();
    let stream_state = server.stream_state().lock();
    let handles = stream_state.handles();
    let mut diagnostics: Vec<String> = Vec::with_capacity(handles.len());
    for handle in handles.values() {
        let handle_info: String = match handle {
            stream::Handle::Available(stream) => match &stream.db {
                Some(db) => db.diagnostics(),
                None => "[BUG] available-but-closed".into(),
            },
            stream::Handle::Acquired => "acquired".into(),
            stream::Handle::Expired => "expired".into(),
        };
        diagnostics.push(handle_info);
    }
    drop(stream_state);

    tracing::trace!("diagnostics: {diagnostics:?}");
    Ok(Json(diagnostics))
}

#[derive(Debug, Deserialize, Serialize)]
struct HttpDatabaseConfig {
    block_reads: bool,
    block_writes: bool,
    #[serde(default)]
    block_reason: Option<String>,
    #[serde(default)]
    max_db_size: Option<bytesize::ByteSize>,
}

async fn handle_post_config<M: MakeNamespace, C>(
    State(app_state): State<Arc<AppState<M, C>>>,
    Path(namespace): Path<String>,
    Json(req): Json<HttpDatabaseConfig>,
) -> crate::Result<()> {
    let store = app_state
        .namespaces
        .config_store(NamespaceName::from_string(namespace)?)
        .await?;
    let mut config = (*store.get()).clone();
    config.block_reads = req.block_reads;
    config.block_writes = req.block_writes;
    config.block_reason = req.block_reason;
    if let Some(size) = req.max_db_size {
        config.max_db_pages = size.as_u64() / LIBSQL_PAGE_SIZE;
    }

    store.store(config)?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct CreateNamespaceReq {
    dump_url: Option<Url>,
    max_db_size: Option<bytesize::ByteSize>,
}

async fn handle_create_namespace<M: MakeNamespace, C: Connector>(
    State(app_state): State<Arc<AppState<M, C>>>,
    Path(namespace): Path<String>,
    Json(req): Json<CreateNamespaceReq>,
) -> crate::Result<()> {
    let dump = match req.dump_url {
        Some(ref url) => {
            RestoreOption::Dump(dump_stream_from_url(url, app_state.connector.clone()).await?)
        }
        None => RestoreOption::Latest,
    };

    let namespace = NamespaceName::from_string(namespace)?;
    app_state.namespaces.create(namespace.clone(), dump).await?;

    if let Some(max_db_size) = req.max_db_size {
        let store = app_state.namespaces.config_store(namespace).await?;
        let mut config = (*store.get()).clone();
        config.max_db_pages = max_db_size.as_u64() / LIBSQL_PAGE_SIZE;
        store.store(config)?;
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct ForkNamespaceReq {
    timestamp: NaiveDateTime,
}

async fn handle_fork_namespace<M: MakeNamespace, C>(
    State(app_state): State<Arc<AppState<M, C>>>,
    Path((from, to)): Path<(String, String)>,
    req: Option<Json<ForkNamespaceReq>>,
) -> crate::Result<()> {
    let timestamp = req.map(|v| v.timestamp);
    let from = NamespaceName::from_string(from)?;
    let to = NamespaceName::from_string(to)?;
    app_state.namespaces.fork(from, to, timestamp).await?;
    Ok(())
}

async fn dump_stream_from_url<C>(url: &Url, connector: C) -> Result<DumpStream, LoadDumpError>
where
    C: Connector,
{
    match url.scheme() {
        "http" | "https" => {
            let client = hyper::client::Client::builder().build::<_, Body>(connector);
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
            let path = PathBuf::from(url.path());
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

async fn handle_delete_namespace<F: MakeNamespace, C>(
    State(app_state): State<Arc<AppState<F, C>>>,
    Path(namespace): Path<String>,
) -> crate::Result<()> {
    app_state
        .namespaces
        .destroy(NamespaceName::from_string(namespace)?)
        .await?;
    Ok(())
}
