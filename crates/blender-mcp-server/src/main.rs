//! Command-line entrypoint for the Scheme-first Blender MCP server.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use blender_mcp_protocol::{
    BridgeOperation, DEFAULT_MAX_FRAME_BYTES, OperatorCatalog, PROTOCOL_VERSION,
};
use blender_mcp_server::{
    http::{self, SecurityPolicy},
    scheme::{SchemeSettings, SchemeWorker},
    server::BlenderMcp,
};
use blender_mcp_transport::{BlenderBridge, HeadlessBridge, HeadlessConfig, LiveBridge};
use clap::{Parser, ValueEnum};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Environment variable consulted when `--token-file` is not supplied.
const TOKEN_ENVIRONMENT_VARIABLE: &str = "BLENDER_MCP_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// Attach to a Blender session that is running the bridge extension.
    Live,
    /// Start and own a `blender --background` child process.
    Headless,
}

#[derive(Debug, Parser)]
#[command(
    name = "blender-mcp",
    version,
    about = "Scheme-first MCP server exposing Blender through a single scheme_eval tool"
)]
struct Cli {
    /// Whether Blender is an already-running session or a process this server owns.
    #[arg(long, value_enum, default_value_t = Backend::Live, env = "BLENDER_MCP_BACKEND")]
    backend: Backend,

    /// Address the MCP Streamable HTTP service binds to.
    #[arg(long, default_value = "127.0.0.1:8000", env = "BLENDER_MCP_BIND")]
    bind: SocketAddr,

    /// Loopback address of the live Blender bridge. Live backend only.
    #[arg(long, default_value = "127.0.0.1:9876", env = "BLENDER_MCP_BRIDGE")]
    bridge: SocketAddr,

    /// Blender executable used by the headless backend.
    #[arg(long, default_value = "blender", env = "BLENDER_MCP_BLENDER")]
    blender: PathBuf,

    /// Installed `scheme_blender_mcp` extension directory. Headless backend only.
    #[arg(long, env = "BLENDER_MCP_EXTENSION_DIR")]
    extension_dir: Option<PathBuf>,

    /// Explicit `headless_bootstrap.py` path. Defaults to the extension directory's parent.
    #[arg(long, env = "BLENDER_MCP_BOOTSTRAP")]
    bootstrap: Option<PathBuf>,

    /// Optional `.blend` loaded before the headless bootstrap runs.
    #[arg(long)]
    blend_file: Option<PathBuf>,

    /// Evaluation budget applied when `scheme_eval` omits `timeout_secs`.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    default_timeout_secs: u64,

    /// Largest `timeout_secs` a caller may request.
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    maximum_timeout_secs: u64,

    /// How long to wait for headless Blender to expose its bridge.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..))]
    startup_timeout_secs: u64,

    /// Blender's `--log-level` for the headless child. Raise it to diagnose startup.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    blender_log_level: i32,

    /// Maximum accepted `POST /mcp` request body size in bytes.
    #[arg(long, default_value_t = http::DEFAULT_MAX_MCP_BODY_BYTES)]
    max_body_bytes: usize,

    /// Permit binding a non-loopback address. Also requires a bearer token and allowed hosts.
    #[arg(long)]
    allow_non_loopback: bool,

    /// File holding the bearer token. Takes precedence over `BLENDER_MCP_TOKEN`.
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// Additional `Host` authority the HTTP service accepts. Repeatable.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,

    /// Additional browser `Origin` the HTTP service accepts. Repeatable.
    #[arg(long = "allowed-origin")]
    allowed_origins: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    initialize_tracing();
    run(cli).await
}

async fn run(cli: Cli) -> Result<()> {
    if cli.default_timeout_secs > cli.maximum_timeout_secs {
        bail!(
            "--default-timeout-secs {} exceeds --maximum-timeout-secs {}",
            cli.default_timeout_secs,
            cli.maximum_timeout_secs
        );
    }
    if cli.backend == Backend::Live && cli.blend_file.is_some() {
        bail!(
            "--blend-file applies to --backend headless; open the file in the live session instead"
        );
    }

    let token = load_token(cli.token_file.as_deref())?;
    let security = security_policy(&cli, token)?;

    let bridge = connect_bridge(&cli).await?;
    let catalog = initial_catalog(&bridge, cli.backend).await?;
    info!(
        blender = %catalog.blender_version,
        operators = catalog.operators.len(),
        revision = %catalog.revision,
        "loaded Blender operator catalog"
    );

    let worker = SchemeWorker::spawn(
        Arc::clone(&bridge),
        catalog,
        tokio::runtime::Handle::current(),
        SchemeSettings {
            default_timeout: Duration::from_secs(cli.default_timeout_secs),
            maximum_timeout: Duration::from_secs(cli.maximum_timeout_secs),
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("failed to start the Steel worker: {error}"))?;

    let shutdown = CancellationToken::new();
    let router = http::router(
        BlenderMcp::new(worker.handle()),
        Arc::clone(&bridge),
        worker.handle(),
        security,
        &shutdown,
        cli.max_body_bytes,
    );

    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("failed to bind {}", cli.bind))?;
    let address = listener
        .local_addr()
        .context("failed to read the bound address")?;
    info!(%address, "blender-mcp is serving POST /mcp and GET /healthz");

    let serve_shutdown = shutdown.clone();
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = terminate_signal() => info!("received a termination signal; draining"),
                () = serve_shutdown.cancelled() => {}
            }
        })
        .await
        .context("the MCP HTTP service failed");

    shutdown.cancel();
    worker.shutdown().await;
    if let Err(error) = bridge.shutdown().await {
        warn!(%error, "the Blender bridge did not shut down cleanly");
    }
    served
}

async fn connect_bridge(cli: &Cli) -> Result<Arc<dyn BlenderBridge>> {
    match cli.backend {
        Backend::Live => {
            info!(bridge = %cli.bridge, "using the live Blender bridge");
            Ok(Arc::new(LiveBridge::new(cli.bridge)))
        }
        Backend::Headless => {
            let config = headless_config(cli)?;
            info!(blender = %config.blender_executable.display(), "starting background Blender");
            let bridge = HeadlessBridge::start(config)
                .await
                .context("failed to start background Blender")?;
            Ok(Arc::new(bridge))
        }
    }
}

fn headless_config(cli: &Cli) -> Result<HeadlessConfig> {
    let extension_dir = cli.extension_dir.clone().context(
        "--extension-dir (or BLENDER_MCP_EXTENSION_DIR) must point at the installed scheme_blender_mcp directory",
    )?;
    if !extension_dir.join("blender_manifest.toml").is_file() {
        bail!(
            "{} is not a scheme_blender_mcp extension directory: blender_manifest.toml is missing",
            extension_dir.display()
        );
    }
    let bootstrap = match cli.bootstrap.clone() {
        Some(bootstrap) => bootstrap,
        None => extension_dir
            .parent()
            .context("--extension-dir has no parent directory; pass --bootstrap explicitly")?
            .join("headless_bootstrap.py"),
    };
    if !bootstrap.is_file() {
        bail!(
            "headless bootstrap script {} does not exist; pass --bootstrap explicitly",
            bootstrap.display()
        );
    }
    Ok(HeadlessConfig {
        blender_executable: cli.blender.clone(),
        bootstrap_script: bootstrap,
        extension_dir,
        blend_file: cli.blend_file.clone(),
        startup_timeout: Duration::from_secs(cli.startup_timeout_secs),
        maximum_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        log_level: cli.blender_log_level,
    })
}

/// Fetch the operator catalog that seeds the generated `bpy/ops/...` Scheme aliases.
///
/// A live Blender session may not have started its bridge yet, so the server comes
/// up with an empty catalog and recovers through `(catalog-refresh!)`. The headless
/// backend already awaited readiness, so a failure there is fatal.
async fn initial_catalog(
    bridge: &Arc<dyn BlenderBridge>,
    backend: Backend,
) -> Result<OperatorCatalog> {
    match bridge
        .request(BridgeOperation::Catalog, Duration::from_secs(120))
        .await
    {
        Ok(response) => {
            let result = response
                .result
                .context("the Blender bridge returned a catalog response with no result")?;
            serde_json::from_value(result)
                .context("the Blender bridge returned a malformed operator catalog")
        }
        Err(error) if backend == Backend::Live => {
            warn!(
                %error,
                "the Blender bridge is not reachable yet; starting with an empty operator catalog. \
                 Start the bridge from Blender's Scheme MCP panel, then evaluate (catalog-refresh!)."
            );
            Ok(empty_catalog())
        }
        Err(error) => {
            Err(anyhow::Error::new(error).context("failed to fetch the headless operator catalog"))
        }
    }
}

fn empty_catalog() -> OperatorCatalog {
    OperatorCatalog {
        protocol_version: PROTOCOL_VERSION,
        revision: "unavailable".to_owned(),
        blender_version: "unavailable".to_owned(),
        operators: Vec::new(),
    }
}

fn load_token(token_file: Option<&Path>) -> Result<Option<String>> {
    if let Some(path) = token_file {
        let token = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read the bearer token file {}", path.display()))?
            .trim()
            .to_owned();
        if token.is_empty() {
            bail!("the bearer token file {} is empty", path.display());
        }
        return Ok(Some(token));
    }
    match std::env::var(TOKEN_ENVIRONMENT_VARIABLE) {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_owned())),
        _ => Ok(None),
    }
}

fn security_policy(cli: &Cli, token: Option<String>) -> Result<SecurityPolicy> {
    let mut hosts = cli.allowed_hosts.clone();
    let mut origins = cli.allowed_origins.clone();
    if cli.bind.ip().is_loopback() {
        let (loopback_hosts, loopback_origins) = http::loopback_authorities(cli.bind.port());
        hosts.extend(loopback_hosts);
        origins.extend(loopback_origins);
    } else {
        if !cli.allow_non_loopback {
            bail!(
                "--bind {} is not a loopback address; pass --allow-non-loopback to accept the exposure",
                cli.bind
            );
        }
        if token.is_none() {
            bail!(
                "non-loopback binding requires a bearer token from --token-file or {TOKEN_ENVIRONMENT_VARIABLE}"
            );
        }
        if hosts.is_empty() {
            bail!("non-loopback binding requires at least one --allowed-host authority");
        }
        warn!(
            bind = %cli.bind,
            "serving plain HTTP off loopback; bearer tokens are only confidential behind a trusted \
             network or an external TLS terminator"
        );
    }
    SecurityPolicy::new(hosts, origins, token).map_err(|message| anyhow::anyhow!(message))
}

async fn terminate_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "failed to listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

fn initialize_tracing() {
    use tracing_subscriber::EnvFilter;

    // Steel logs every module resolution at INFO, which buries our own startup lines.
    let filter = EnvFilter::try_from_env("BLENDER_MCP_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,steel=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(arguments: &[&str]) -> Cli {
        let mut all = vec!["blender-mcp"];
        all.extend_from_slice(arguments);
        Cli::try_parse_from(all).expect("arguments parse")
    }

    #[test]
    fn loopback_defaults_need_no_token_or_explicit_hosts() {
        let policy = security_policy(&cli(&[]), None).expect("loopback policy");
        assert!(
            policy
                .allowed_hosts()
                .contains(&"127.0.0.1:8000".to_owned())
        );
        assert!(
            policy
                .allowed_origins()
                .contains(&"http://localhost:8000".to_owned())
        );
    }

    #[test]
    fn non_loopback_requires_opt_in_token_and_hosts() {
        let exposed = cli(&["--bind", "0.0.0.0:8000"]);
        assert!(security_policy(&exposed, None).is_err());

        let opted_in = cli(&["--bind", "0.0.0.0:8000", "--allow-non-loopback"]);
        assert!(security_policy(&opted_in, None).is_err());
        assert!(security_policy(&opted_in, Some("token".to_owned())).is_err());

        let complete = cli(&[
            "--bind",
            "0.0.0.0:8000",
            "--allow-non-loopback",
            "--allowed-host",
            "studio.internal:8000",
        ]);
        let policy =
            security_policy(&complete, Some("token".to_owned())).expect("non-loopback policy");
        assert_eq!(
            policy.allowed_hosts(),
            vec!["studio.internal:8000".to_owned()]
        );
    }

    #[test]
    fn extra_authorities_extend_rather_than_replace_loopback() {
        let policy =
            security_policy(&cli(&["--allowed-host", "blender.local:8000"]), None).expect("policy");
        let hosts = policy.allowed_hosts();
        assert!(hosts.contains(&"blender.local:8000".to_owned()));
        assert!(hosts.contains(&"127.0.0.1:8000".to_owned()));
    }

    #[test]
    fn token_file_wins_over_the_environment_and_rejects_empty_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("token");
        std::fs::write(&path, "  file-token\n").expect("write token");
        assert_eq!(
            load_token(Some(&path)).expect("token"),
            Some("file-token".to_owned())
        );

        std::fs::write(&path, "   \n").expect("write blank token");
        assert!(load_token(Some(&path)).is_err());
    }

    #[test]
    fn rejects_a_default_timeout_above_the_maximum() {
        assert!(
            Cli::try_parse_from(["blender-mcp", "--default-timeout-secs", "0"]).is_err(),
            "zero timeouts must be rejected by the parser"
        );
    }

    #[test]
    fn headless_config_requires_a_real_extension_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = cli(&[
            "--backend",
            "headless",
            "--extension-dir",
            directory.path().to_str().expect("utf-8 path"),
        ]);
        assert!(headless_config(&missing).is_err());
    }
}
