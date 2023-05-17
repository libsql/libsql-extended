use std::{
    env,
    fs::{self, OpenOptions},
    io::{stdout, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use sqld::{database::dump::exporter::export_dump, Config};
use tracing_subscriber::filter::LevelFilter;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// SQL daemon
#[derive(Debug, Parser)]
#[command(name = "sqld")]
#[command(about = "SQL daemon", version, long_about = None)]
struct Cli {
    #[clap(long, short, default_value = "data.sqld", env = "SQLD_DB_PATH")]
    db_path: PathBuf,

    /// The directory path where trusted extensions can be loaded from.
    /// If not present, extension loading is disabled.
    /// If present, the directory is expected to have a trusted.lst file containing
    /// the sha256 and name of each extension, one per line. Example:
    ///
    /// 99890762817735984843bf5cf02a4b2ea648018fd05f04df6f9ce7f976841510  math.dylib
    #[clap(long, short)]
    extensions_path: Option<PathBuf>,

    /// The address and port the PostgreSQL server listens to.
    #[clap(long, short, env = "SQLD_PG_LISTEN_ADDR")]
    pg_listen_addr: Option<SocketAddr>,

    #[clap(long, default_value = "127.0.0.1:8080", env = "SQLD_HTTP_LISTEN_ADDR")]
    http_listen_addr: SocketAddr,
    #[clap(long)]
    enable_http_console: bool,

    /// The address and port the Hrana server listens to.
    #[clap(long, short = 'l', env = "SQLD_HRANA_LISTEN_ADDR")]
    hrana_listen_addr: Option<SocketAddr>,

    /// Path to a file with a JWT decoding key used to authenticate clients in the Hrana and HTTP
    /// APIs. The key is either a PKCS#8-encoded Ed25519 public key in PEM, or just plain bytes of
    /// the Ed25519 public key in URL-safe base64.
    ///
    /// You can also pass the key directly in the env variable SQLD_AUTH_JWT_KEY.
    #[clap(long, env = "SQLD_AUTH_JWT_KEY_FILE")]
    auth_jwt_key_file: Option<PathBuf>,
    /// Specifies legacy HTTP basic authentication. The argument must be in format "basic:$PARAM",
    /// where $PARAM is base64-encoded string "$USERNAME:$PASSWORD".
    #[clap(long, env = "SQLD_HTTP_AUTH")]
    http_auth: Option<String>,

    /// The address and port the inter-node RPC protocol listens to. Example: `0.0.0.0:5001`.
    #[clap(
        long,
        conflicts_with = "primary_grpc_url",
        env = "SQLD_GRPC_LISTEN_ADDR"
    )]
    grpc_listen_addr: Option<SocketAddr>,
    #[clap(
        long,
        requires = "grpc_cert_file",
        requires = "grpc_key_file",
        requires = "grpc_ca_cert_file"
    )]
    grpc_tls: bool,
    #[clap(long)]
    grpc_cert_file: Option<PathBuf>,
    #[clap(long)]
    grpc_key_file: Option<PathBuf>,
    #[clap(long)]
    grpc_ca_cert_file: Option<PathBuf>,

    /// The gRPC URL of the primary node to connect to for writes. Example: `http://localhost:5001`.
    #[clap(long, env = "SQLD_PRIMARY_GRPC_URL")]
    primary_grpc_url: Option<String>,
    #[clap(
        long,
        requires = "primary_grpc_cert_file",
        requires = "primary_grpc_key_file",
        requires = "primary_grpc_ca_cert_file"
    )]
    primary_grpc_tls: bool,
    #[clap(long)]
    primary_grpc_cert_file: Option<PathBuf>,
    #[clap(long)]
    primary_grpc_key_file: Option<PathBuf>,
    #[clap(long)]
    primary_grpc_ca_cert_file: Option<PathBuf>,

    #[clap(
        long,
        short,
        value_enum,
        default_value = "libsql",
        env = "SQLD_BACKEND"
    )]
    backend: sqld::Backend,
    // The url to connect with mWAL backend, based on mvSQLite
    #[cfg(feature = "mwal_backend")]
    #[clap(long, short, env = "SQLD_MWAL_ADDR")]
    mwal_addr: Option<String>,

    /// Don't display welcome message
    #[clap(long)]
    no_welcome: bool,
    #[cfg(feature = "bottomless")]
    #[clap(long, env = "SQLD_ENABLE_BOTTOMLESS_REPLICATION")]
    enable_bottomless_replication: bool,
    /// The duration, in second, after which to shutdown the server if no request have been
    /// received.
    /// By default, the server doesn't shutdown when idle.
    #[clap(long, env = "SQLD_IDLE_SHUTDOWN_TIMEOUT_S")]
    idle_shutdown_timeout_s: Option<u64>,

    /// Load the dump at the provided path.
    /// Requires that the node is not in replica mode
    #[clap(long, env = "SQLD_LOAD_DUMP_PATH", conflicts_with = "primary_grpc_url")]
    load_from_dump: Option<PathBuf>,

    /// Maximum size the replication log is allowed to grow (in MB).
    /// defaults to 200MB.
    #[clap(long, env = "SQLD_MAX_LOG_SIZE", default_value = "200")]
    max_log_size: u64,

    #[clap(subcommand)]
    utils: Option<UtilsSubcommands>,

    /// The URL to send a server heartbeat `POST` request to.
    /// By default, the server doesn't send a heartbeat.
    #[clap(long, env = "SQLD_HEARTBEAT_URL")]
    heartbeat_url: Option<String>,

    /// The HTTP "Authornization" header to include in the a server heartbeat
    /// `POST` request.
    /// By default, the server doesn't send a heartbeat.
    #[clap(long, env = "SQLD_HEARTBEAT_AUTH")]
    heartbeat_auth: Option<String>,

    /// The heartbeat time period in seconds.
    /// By default, the the period is 30 seconds.
    #[clap(long, env = "SQLD_HEARTBEAT_PERIOD_S", default_value = "30")]
    heartbeat_period_s: u64,
}

#[derive(clap::Subcommand, Debug)]
enum UtilsSubcommands {
    Dump {
        #[clap(long)]
        /// Path at which to write the dump
        path: Option<PathBuf>,
    },
}

impl Cli {
    #[rustfmt::skip]
    fn print_welcome_message(&self) {
        // no welcome :'(
        if self.no_welcome { return }

        eprintln!(r#"           _     _ "#);
        eprintln!(r#" ___  __ _| | __| |"#);
        eprintln!(r#"/ __|/ _` | |/ _` |"#);
        eprintln!(r#"\__ \ (_| | | (_| |"#);
        eprintln!(r#"|___/\__, |_|\__,_|"#);
        eprintln!(r#"        |_|        "#);

        eprintln!();
        eprintln!("Welcome to sqld!");
        eprintln!();
        eprintln!("version: {}", env!("VERGEN_BUILD_SEMVER"));
        if let Some(git_sha) = option_env!("VERGEN_GIT_SHA") {
            eprintln!("commit SHA: {git_sha}");
        }
        eprintln!("build date: {}", env!("VERGEN_BUILD_DATE"));
        eprintln!();
        eprintln!("This software is in BETA version.");
        eprintln!("If you encounter any bug, please open an issue at https://github.com/libsql/sqld/issues");
        eprintln!();

        eprintln!("config:");

        eprint!("\t- mode: ");
        match (&self.grpc_listen_addr, &self.primary_grpc_url) {
            (None, None) => eprintln!("standalone"),
            (Some(addr), None) => eprintln!("primary ({addr})"),
            (None, Some(url)) => eprintln!("replica (primary at {url})"),
            _ => unreachable!("invalid configuration!"),
        };
        eprintln!("\t- database path: {}", self.db_path.display());
        let extensions_str = self.extensions_path.clone().map_or("<disabled>".to_string(), |x| x.display().to_string());
        eprintln!("\t- extensions path: {extensions_str}");
        eprintln!("\t- listening for HTTP requests on: {}", self.http_listen_addr);
        if let Some(ref addr) = self.pg_listen_addr {
            eprintln!("\t- listening for PostgreSQL wire on: {addr}");
        }
        eprintln!("\t- grpc_tls: {}", if self.grpc_tls { "yes" } else { "no" });
    }
}

fn config_from_args(args: Cli) -> Result<Config> {
    let auth_jwt_key = if let Some(file_path) = args.auth_jwt_key_file {
        let data = fs::read_to_string(file_path).context("Could not read file with JWT key")?;
        Some(data)
    } else {
        match env::var("SQLD_AUTH_JWT_KEY") {
            Ok(key) => Some(key),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                bail!("Env variable SQLD_AUTH_JWT_KEY does not contain a valid Unicode value")
            }
        }
    };

    Ok(Config {
        db_path: args.db_path,
        extensions_path: args.extensions_path,
        tcp_addr: args.pg_listen_addr,
        http_addr: Some(args.http_listen_addr),
        enable_http_console: args.enable_http_console,
        hrana_addr: args.hrana_listen_addr,
        auth_jwt_key,
        http_auth: args.http_auth,
        backend: args.backend,
        writer_rpc_addr: args.primary_grpc_url,
        writer_rpc_tls: args.primary_grpc_tls,
        writer_rpc_cert: args.primary_grpc_cert_file,
        writer_rpc_key: args.primary_grpc_key_file,
        writer_rpc_ca_cert: args.primary_grpc_ca_cert_file,
        rpc_server_addr: args.grpc_listen_addr,
        rpc_server_tls: args.grpc_tls,
        rpc_server_cert: args.grpc_cert_file,
        rpc_server_key: args.grpc_key_file,
        rpc_server_ca_cert: args.grpc_ca_cert_file,
        #[cfg(feature = "mwal_backend")]
        mwal_addr: args.mwal_addr,
        #[cfg(feature = "bottomless")]
        enable_bottomless_replication: args.enable_bottomless_replication,
        idle_shutdown_timeout: args.idle_shutdown_timeout_s.map(Duration::from_secs),
        load_from_dump: args.load_from_dump,
        max_log_size: args.max_log_size,
        heartbeat_url: args.heartbeat_url,
        heartbeat_auth: args.heartbeat_auth,
        heartbeat_period: Duration::from_secs(args.heartbeat_period_s),
    })
}

fn perform_dump(dump_path: Option<&Path>, db_path: &Path) -> anyhow::Result<()> {
    let out: Box<dyn Write> = match dump_path {
        Some(path) => {
            let f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .with_context(|| format!("file `{}` already exists", path.display()))?;
            Box::new(f)
        }
        None => Box::new(stdout()),
    };
    let conn = rusqlite::Connection::open(db_path.join("data"))?;

    export_dump(conn, out)?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    let args = Cli::parse();

    match args.utils {
        Some(UtilsSubcommands::Dump { path }) => {
            if let Some(ref path) = path {
                eprintln!(
                    "Dumping database {} to {}",
                    args.db_path.display(),
                    path.display()
                );
            }
            perform_dump(path.as_deref(), &args.db_path)
        }
        None => {
            args.print_welcome_message();
            #[cfg(feature = "mwal_backend")]
            match (&args.backend, args.mwal_addr.is_some()) {
                (sqld::Backend::Mwal, false) => {
                    anyhow::bail!("--mwal-addr parameter must be present with mwal backend")
                }
                (backend, true) if backend != &sqld::Backend::Mwal => {
                    anyhow::bail!(
                        "--mwal-addr parameter conflicts with backend {:?}",
                        args.backend
                    )
                }
                _ => (),
            }

            let config = config_from_args(args)?;
            sqld::run_server(config).await?;

            Ok(())
        }
    }
}
