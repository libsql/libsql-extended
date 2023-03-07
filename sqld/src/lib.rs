use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "mwal_backend")]
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as AnyhowContext;
use database::libsql::LibSqlDb;
use database::service::DbFactoryService;
use database::write_proxy::WriteProxyDbFactory;
use once_cell::sync::Lazy;
#[cfg(feature = "mwal_backend")]
use once_cell::sync::OnceCell;
use replication::logger::{ReplicationLogger, ReplicationLoggerHook};
use rpc::run_rpc_server;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tower::load::Constant;
use tower::ServiceExt;
use utils::services::idle_shutdown::IdleShutdownLayer;

use crate::auth::Auth;
use crate::error::Error;
use crate::postgres::service::PgConnectionFactory;
use crate::server::Server;

pub use sqld_libsql_bindings as libsql;

mod auth;
mod database;
mod error;
mod hrana;
mod http;
mod postgres;
mod query;
mod query_analysis;
mod replication;
pub mod rpc;
mod server;
mod utils;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum Backend {
    Libsql,
    #[cfg(feature = "mwal_backend")]
    Mwal,
}

type Result<T> = std::result::Result<T, Error>;

/// Trigger a hard database reset. This cause the database to be wiped, freshly restarted
/// This is used for replicas that are left in an unrecoverabe state and should restart from a
/// fresh state.
///
/// /!\ use with caution.
pub(crate) static HARD_RESET: Lazy<Arc<Notify>> = Lazy::new(|| Arc::new(Notify::new()));

#[cfg(feature = "mwal_backend")]
pub(crate) static VWAL_METHODS: OnceCell<
    Option<Arc<Mutex<sqld_libsql_bindings::mwal::ffi::libsql_wal_methods>>>,
> = OnceCell::new();

pub struct Config {
    pub db_path: PathBuf,
    pub tcp_addr: Option<SocketAddr>,
    pub ws_addr: Option<SocketAddr>,
    pub http_addr: Option<SocketAddr>,
    pub enable_http_console: bool,
    pub http_auth: Option<String>,
    pub hrana_addr: Option<SocketAddr>,
    pub auth_jwt_key: Option<String>,
    pub backend: Backend,
    #[cfg(feature = "mwal_backend")]
    pub mwal_addr: Option<String>,
    pub writer_rpc_addr: Option<String>,
    pub writer_rpc_tls: bool,
    pub writer_rpc_cert: Option<PathBuf>,
    pub writer_rpc_key: Option<PathBuf>,
    pub writer_rpc_ca_cert: Option<PathBuf>,
    pub rpc_server_addr: Option<SocketAddr>,
    pub rpc_server_tls: bool,
    pub rpc_server_cert: Option<PathBuf>,
    pub rpc_server_key: Option<PathBuf>,
    pub rpc_server_ca_cert: Option<PathBuf>,
    #[cfg(feature = "bottomless")]
    pub enable_bottomless_replication: bool,
    pub create_local_http_tunnel: bool,
    pub idle_shutdown_timeout: Option<Duration>,
}

async fn run_service(
    service: DbFactoryService,
    config: &Config,
    join_set: &mut JoinSet<anyhow::Result<()>>,
    idle_shutdown_layer: Option<IdleShutdownLayer>,
) -> anyhow::Result<()> {
    let auth = get_auth(config)?;

    let mut server = Server::new();
    if let Some(addr) = config.tcp_addr {
        server.bind_tcp(addr).await?;
    }
    if let Some(addr) = config.ws_addr {
        server.bind_ws(addr).await?;
    }

    let factory = PgConnectionFactory::new(service.clone());
    join_set.spawn(server.serve(factory));

    let (upgrade_tx, upgrade_rx) = mpsc::channel(8);

    if let Some(addr) = config.http_addr {
        join_set.spawn(http::run_http(
            addr,
            auth.clone(),
            service.clone().map_response(|s| Constant::new(s, 1)),
            upgrade_tx,
            config.enable_http_console,
            idle_shutdown_layer.clone(),
        ));
    }

    if let Some(addr) = config.hrana_addr {
        join_set.spawn(async move {
            hrana::serve(service.factory, auth, idle_shutdown_layer, addr, upgrade_rx)
                .await
                .context("Hrana server failed")
        });
    }

    Ok(())
}

fn get_auth(config: &Config) -> anyhow::Result<Arc<Auth>> {
    let mut auth = Auth::default();

    if let Some(arg) = config.http_auth.as_deref() {
        let param = auth::parse_http_basic_auth_arg(arg)?;
        auth.http_basic = Some(param);
        tracing::info!("Using legacy HTTP basic authentication");
    }

    if let Some(jwt_key) = config.auth_jwt_key.as_deref() {
        let jwt_key = auth::parse_jwt_key(jwt_key).context("Could not parse JWT decoding key")?;
        auth.jwt_key = Some(jwt_key);
        tracing::info!("Using JWT-based authentication");
    }

    auth.disabled = auth.http_basic.is_none() && auth.jwt_key.is_none();
    if auth.disabled {
        tracing::warn!("No authentication specified, the server will not require authentication")
    }

    Ok(Arc::new(auth))
}

/// nukes current DB and start anew
async fn hard_reset(
    config: &Config,
    mut join_set: JoinSet<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    tracing::error!("received hard-reset command: reseting replica.");

    tracing::info!("Shutting down all services...");
    join_set.shutdown().await;
    tracing::info!("All services have been shut down.");

    let db_path = &config.db_path;
    tokio::fs::remove_dir_all(db_path).await?;

    Ok(())
}

async fn start_primary(
    config: &Config,
    join_set: &mut JoinSet<anyhow::Result<()>>,
    addr: &str,
    idle_shutdown_layer: Option<IdleShutdownLayer>,
) -> anyhow::Result<()> {
    let (factory, handle) = WriteProxyDbFactory::new(
        addr,
        config.writer_rpc_tls,
        config.writer_rpc_cert.clone(),
        config.writer_rpc_key.clone(),
        config.writer_rpc_ca_cert.clone(),
        config.db_path.clone(),
    )
    .await
    .context("failed to start WriteProxy DB")?;

    // the `JoinSet` does not support inserting arbitrary `JoinHandles` (and it also does not
    // support spawning blocking tasks, so we must spawn a proxy task to add the `handle` to
    // `join_set`
    join_set.spawn(async move { handle.await.expect("WriteProxy DB task failed") });

    let service = DbFactoryService::new(Arc::new(factory));
    run_service(service, config, join_set, idle_shutdown_layer).await?;

    Ok(())
}

async fn start_replica(
    config: &Config,
    join_set: &mut JoinSet<anyhow::Result<()>>,
    idle_shutdown_layer: Option<IdleShutdownLayer>,
) -> anyhow::Result<()> {
    let logger = Arc::new(ReplicationLogger::open(&config.db_path)?);
    let logger_clone = logger.clone();
    let path_clone = config.db_path.clone();
    #[cfg(feature = "bottomless")]
    let enable_bottomless = config.enable_bottomless_replication;
    #[cfg(not(feature = "bottomless"))]
    let enable_bottomless = false;
    let db_factory = Arc::new(move || {
        let db_path = path_clone.clone();
        let hook = ReplicationLoggerHook::new(logger.clone());
        async move { LibSqlDb::new(db_path, hook, enable_bottomless) }
    });
    let service = DbFactoryService::new(db_factory.clone());
    if let Some(ref addr) = config.rpc_server_addr {
        join_set.spawn(run_rpc_server(
            *addr,
            config.rpc_server_tls,
            config.rpc_server_cert.clone(),
            config.rpc_server_key.clone(),
            config.rpc_server_ca_cert.clone(),
            db_factory,
            logger_clone,
            idle_shutdown_layer.clone(),
        ));
    }

    run_service(service, config, join_set, idle_shutdown_layer).await?;

    Ok(())
}

pub async fn run_server(config: Config) -> anyhow::Result<()> {
    tracing::trace!("Backend: {:?}", config.backend);

    #[cfg(feature = "mwal_backend")]
    {
        if config.backend == Backend::Mwal {
            std::env::set_var("MVSQLITE_DATA_PLANE", config.mwal_addr.as_ref().unwrap());
        }
        VWAL_METHODS
            .set(config.mwal_addr.as_ref().map(|_| {
                Arc::new(Mutex::new(
                    sqld_libsql_bindings::mwal::ffi::libsql_wal_methods::new(),
                ))
            }))
            .map_err(|_| anyhow::anyhow!("wal_methods initialized twice"))?;
    }

    #[cfg(feature = "bottomless")]
    if config.enable_bottomless_replication {
        bottomless::static_init::register_bottomless_methods();
    }

    let (local_tunnel_shutdown, _) = localtunnel_client::broadcast::channel(1);
    if config.create_local_http_tunnel {
        let tunnel = localtunnel_client::open_tunnel(
            Some("https://localtunnel.me"),
            None,
            config.http_addr.map(|a| a.ip().to_string()).as_deref(),
            config.http_addr.map(|a| a.port()).unwrap_or(8080),
            local_tunnel_shutdown.clone(),
            3,
            None,
        )
        .await?;
        println!("HTTP tunnel created: {tunnel}");
    }

    loop {
        if !config.db_path.exists() {
            std::fs::create_dir_all(&config.db_path)?;
        }
        let mut join_set = JoinSet::new();

        let shutdown_notify: Arc<Notify> = Arc::new(Notify::new());
        let idle_shutdown_layer = config
            .idle_shutdown_timeout
            .map(|d| IdleShutdownLayer::new(d, shutdown_notify.clone()));

        match config.writer_rpc_addr {
            Some(ref addr) => {
                start_primary(&config, &mut join_set, addr, idle_shutdown_layer).await?
            }
            None => start_replica(&config, &mut join_set, idle_shutdown_layer).await?,
        }

        let reset = HARD_RESET.clone();
        loop {
            tokio::select! {
                _ = reset.notified() => {
                    hard_reset(&config, join_set).await?;
                    break;
                },
                _ = shutdown_notify.notified() => {
                    return Ok(())
                }
                Some(res) = join_set.join_next() => {
                    res??;
                },
                else => return Ok(()),
            }
        }
    }
}
