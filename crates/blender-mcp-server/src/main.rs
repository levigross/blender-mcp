//! Command-line entrypoint for the Scheme-first Blender MCP server.

use std::{
    collections::HashSet,
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
    sessions::{DEFAULT_SESSION, MAX_SESSIONS, Sessions, validate_name},
};
use blender_mcp_transport::{BlenderBridge, HeadlessBridge, HeadlessConfig, LiveBridge};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
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

    /// JSON file describing additional named live or headless Blender sessions.
    #[arg(long, env = "BLENDER_MCP_SESSIONS_FILE")]
    sessions_file: Option<PathBuf>,

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

    let configurations = session_configurations(&cli)?;
    let shutdown = CancellationToken::new();
    let _cancel_on_exit = shutdown.clone().drop_guard();
    install_shutdown_listener(shutdown.clone()).await?;
    // Bind before starting children so a busy port cannot leave background Blender running.
    let listener = tokio::net::TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("failed to bind {}", cli.bind))?;
    let address = listener
        .local_addr()
        .context("failed to read the bound address")?;
    let running = start_sessions(&cli, configurations, &shutdown).await?;
    let default = &running[0];
    let mut sessions = Sessions::new(default.worker.handle());
    for session in running.iter().skip(1) {
        if let Err(error) = sessions.insert(session.name.clone(), session.worker.handle()) {
            shutdown_sessions(running).await;
            bail!("failed to register Blender session: {error}");
        }
    }

    let router = http::router(
        BlenderMcp::with_sessions(sessions),
        Arc::clone(&default.bridge),
        default.worker.handle(),
        security,
        &shutdown,
        cli.max_body_bytes,
    );

    info!(%address, "blender-mcp is serving POST /mcp and GET /healthz");

    let serve_shutdown = shutdown.clone();
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            serve_shutdown.cancelled().await;
        })
        .await
        .context("the MCP HTTP service failed");

    shutdown.cancel();
    shutdown_sessions(running).await;
    served
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
enum SessionDefinition {
    Live {
        name: String,
        bridge: SocketAddr,
    },
    Headless {
        name: String,
        #[serde(default)]
        blend_file: Option<PathBuf>,
    },
}

impl SessionDefinition {
    fn name(&self) -> &str {
        match self {
            Self::Live { name, .. } | Self::Headless { name, .. } => name,
        }
    }
}

#[derive(Debug)]
enum SessionBackend {
    Live(SocketAddr),
    Headless(HeadlessConfig),
}

#[derive(Debug)]
struct SessionConfiguration {
    name: String,
    backend: SessionBackend,
}

fn session_configurations(cli: &Cli) -> Result<Vec<SessionConfiguration>> {
    let (definitions, directory) = match &cli.sessions_file {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read sessions file {}", path.display()))?;
            let definitions: Vec<SessionDefinition> = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid sessions file {}", path.display()))?;
            (definitions, path.parent().unwrap_or_else(|| Path::new(".")))
        }
        None => (Vec::new(), Path::new(".")),
    };
    prepare_sessions(cli, definitions, directory)
}

fn prepare_sessions(
    cli: &Cli,
    definitions: Vec<SessionDefinition>,
    directory: &Path,
) -> Result<Vec<SessionConfiguration>> {
    if definitions.len() >= MAX_SESSIONS {
        bail!("at most {MAX_SESSIONS} Blender sessions are allowed, including default");
    }
    let mut names = HashSet::from([DEFAULT_SESSION.to_owned()]);
    let mut addresses = HashSet::new();
    if cli.backend == Backend::Live {
        register_live_address(&mut addresses, cli.bridge)?;
    }
    for definition in &definitions {
        validate_name(definition.name()).map_err(anyhow::Error::msg)?;
        if !names.insert(definition.name().to_owned()) {
            bail!(
                "duplicate or reserved Blender session name: {}",
                definition.name()
            );
        }
        if let SessionDefinition::Live { bridge, .. } = definition {
            register_live_address(&mut addresses, *bridge)?;
        }
    }

    let mut configurations = vec![SessionConfiguration {
        name: DEFAULT_SESSION.to_owned(),
        backend: match cli.backend {
            Backend::Live => SessionBackend::Live(cli.bridge),
            Backend::Headless => SessionBackend::Headless(headless_config(cli)?),
        },
    }];
    for definition in definitions {
        let name = definition.name().to_owned();
        let backend = match definition {
            SessionDefinition::Live { bridge, .. } => SessionBackend::Live(bridge),
            SessionDefinition::Headless { blend_file, .. } => {
                let blend_file = blend_file.map(|path| directory.join(path));
                SessionBackend::Headless(headless_config_for(cli, blend_file)?)
            }
        };
        configurations.push(SessionConfiguration { name, backend });
    }
    Ok(configurations)
}

fn register_live_address(addresses: &mut HashSet<SocketAddr>, address: SocketAddr) -> Result<()> {
    let canonical = SocketAddr::new(address.ip().to_canonical(), address.port());
    if !canonical.ip().is_loopback() {
        bail!("live Blender bridge {address} must use a loopback address");
    }
    if !addresses.insert(canonical) {
        bail!("multiple Blender sessions cannot use the same live bridge: {address}");
    }
    Ok(())
}

struct RunningSession {
    name: String,
    bridge: Arc<dyn BlenderBridge>,
    worker: SchemeWorker,
}

async fn start_sessions(
    cli: &Cli,
    configurations: Vec<SessionConfiguration>,
    shutdown: &CancellationToken,
) -> Result<Vec<RunningSession>> {
    let mut running = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        match start_session(cli, configuration, shutdown).await {
            Ok(session) => running.push(session),
            Err(error) => {
                shutdown_sessions(running).await;
                return Err(error);
            }
        }
    }
    Ok(running)
}

async fn start_session(
    cli: &Cli,
    configuration: SessionConfiguration,
    shutdown: &CancellationToken,
) -> Result<RunningSession> {
    if shutdown.is_cancelled() {
        bail!("Blender startup cancelled");
    }
    let name = configuration.name;
    let (bridge, backend): (Arc<dyn BlenderBridge>, _) = match configuration.backend {
        SessionBackend::Live(address) => {
            info!(session = %name, bridge = %address, "using the live Blender bridge");
            (Arc::new(LiveBridge::new(address)), Backend::Live)
        }
        SessionBackend::Headless(config) => {
            info!(session = %name, blender = %config.blender_executable.display(), "starting background Blender");
            // HeadlessBridge owns its child before the first readiness await.
            // Dropping this future kills the child and aborts its log pumps.
            let bridge = tokio::select! {
                biased;
                () = shutdown.cancelled() => bail!("Blender startup cancelled"),
                bridge = HeadlessBridge::start(config) => bridge,
            }
            .with_context(|| format!("failed to start background Blender for session {name}"))?;
            (Arc::new(bridge), Backend::Headless)
        }
    };
    let started = async {
        let catalog = tokio::select! {
            biased;
            () = shutdown.cancelled() => bail!("Blender startup cancelled"),
            catalog = initial_catalog(&bridge, backend) => catalog?,
        };
        info!(
            session = %name,
            blender = %catalog.blender_version,
            operators = catalog.operators.len(),
            revision = %catalog.revision,
            "loaded Blender operator catalog"
        );
        // Worker initialization owns a native thread: let it finish so shutdown
        // can join the thread, even when a signal arrives during initialization.
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
        .map_err(|error| {
            anyhow::anyhow!("failed to start the Steel worker for session {name}: {error}")
        })?;
        if shutdown.is_cancelled() {
            worker.shutdown().await;
            bail!("Blender startup cancelled");
        }
        Ok(worker)
    }
    .await;
    match started {
        Ok(worker) => Ok(RunningSession {
            name,
            bridge,
            worker,
        }),
        Err(error) => {
            if let Err(shutdown_error) = bridge.shutdown().await {
                warn!(session = %name, %shutdown_error, "failed to clean up the Blender bridge after startup failed");
            }
            Err(error)
        }
    }
}

async fn shutdown_sessions(sessions: Vec<RunningSession>) {
    for session in sessions.into_iter().rev() {
        session.worker.shutdown().await;
        if let Err(error) = session.bridge.shutdown().await {
            warn!(session = %session.name, %error, "the Blender bridge did not shut down cleanly");
        }
    }
}

fn headless_config(cli: &Cli) -> Result<HeadlessConfig> {
    headless_config_for(cli, cli.blend_file.clone())
}

fn headless_config_for(cli: &Cli, blend_file: Option<PathBuf>) -> Result<HeadlessConfig> {
    if let Some(path) = &blend_file
        && !path.is_file()
    {
        bail!("Blender scene file {} does not exist", path.display());
    }
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
        blend_file,
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

async fn install_shutdown_listener(shutdown: CancellationToken) -> Result<()> {
    #[cfg(unix)]
    let signal = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt =
            signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
        let mut terminate =
            signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
        async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        }
    };
    #[cfg(not(unix))]
    let signal = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "failed to listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = signal => {
                info!("received a termination signal; draining");
                shutdown.cancel();
            }
            () = async {
                // The biased select polls signal first, installing even lazy
                // platform handlers before startup is allowed to spawn children.
                let _ = ready_tx.send(());
                shutdown.cancelled().await;
            } => {}
        }
    });
    ready_rx
        .await
        .context("termination signal interrupted startup")
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

    fn live_definition(name: &str, port: u16) -> SessionDefinition {
        SessionDefinition::Live {
            name: name.to_owned(),
            bridge: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    #[tokio::test]
    async fn startup_cancellation_interrupts_a_pending_catalog_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bridge listener");
        let address = listener.local_addr().expect("bridge address");
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let startup = tokio::spawn(async move {
            start_session(
                &cli(&["--backend", "live"]),
                SessionConfiguration {
                    name: "waiting".to_owned(),
                    backend: SessionBackend::Live(address),
                },
                &task_shutdown,
            )
            .await
        });
        let connected = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await;
        // Keep the connection open without replying, simulating Blender stuck
        // during startup, then cancel while its request is pending.
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), startup)
            .await
            .expect("startup cancellation does not wait for the catalog timeout")
            .expect("startup joins");
        assert!(connected.is_ok(), "startup contacted the bridge");
        assert!(matches!(result, Err(error) if error.to_string().contains("startup cancelled")));
    }

    #[tokio::test]
    async fn cancelled_startup_does_not_connect_to_the_next_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bridge listener");
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let result = start_sessions(
            &cli(&["--backend", "live"]),
            vec![SessionConfiguration {
                name: "never-started".to_owned(),
                backend: SessionBackend::Live(listener.local_addr().expect("address")),
            }],
            &shutdown,
        )
        .await;
        assert!(result.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "cancelled startup must not connect to another bridge"
        );
    }

    #[test]
    fn session_names_and_capacity_are_checked_before_startup() {
        let cli = cli(&["--backend", "live", "--bridge", "127.0.0.1:9876"]);
        for name in ["", "default", "../escape", "a b", "é"] {
            assert!(
                prepare_sessions(&cli, vec![live_definition(name, 9877)], Path::new(".")).is_err()
            );
        }
        let too_long = "a".repeat(65);
        assert!(
            prepare_sessions(&cli, vec![live_definition(&too_long, 9877)], Path::new(".")).is_err()
        );
        assert!(
            prepare_sessions(
                &cli,
                vec![live_definition("a", 9877), live_definition("a", 9878)],
                Path::new(".")
            )
            .is_err()
        );
        let definitions = (0..15)
            .map(|index| live_definition(&format!("game-{index}"), 9900 + index))
            .collect();
        assert_eq!(
            prepare_sessions(&cli, definitions, Path::new("."))
                .expect("capacity boundary")
                .len(),
            MAX_SESSIONS
        );
        let definitions = (0..16)
            .map(|index| live_definition(&format!("game-{index}"), 9900 + index))
            .collect();
        assert!(prepare_sessions(&cli, definitions, Path::new(".")).is_err());
    }

    #[test]
    fn duplicate_and_non_loopback_bridges_are_rejected() {
        let cli = cli(&["--backend", "live", "--bridge", "127.0.0.1:9876"]);
        assert!(
            prepare_sessions(&cli, vec![live_definition("editor", 9876)], Path::new(".")).is_err()
        );
        assert!(
            prepare_sessions(
                &cli,
                vec![live_definition("a", 9877), live_definition("b", 9877)],
                Path::new(".")
            )
            .is_err()
        );
        for address in ["[::ffff:127.0.0.1]:9876", "192.0.2.1:9877"] {
            let definition = SessionDefinition::Live {
                name: "editor".to_owned(),
                bridge: address.parse().expect("address"),
            };
            assert!(prepare_sessions(&cli, vec![definition], Path::new(".")).is_err());
        }
    }

    #[test]
    fn sessions_file_rejects_unknown_fields_and_invalid_backend_records() {
        for document in [
            r#"[{"name":"editor","backend":"live"}]"#,
            r#"[{"name":"game","backend":"headless","bridge":"127.0.0.1:9877"}]"#,
            r#"[{"name":"game","backend":"headless","blend_flie":"game.blend"}]"#,
            r#"[{"name":"game","backend":"unknown"}]"#,
        ] {
            assert!(serde_json::from_str::<Vec<SessionDefinition>>(document).is_err());
        }
    }

    #[test]
    fn headless_sessions_resolve_scene_paths_without_inheriting_the_default_scene() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let extension = directory.path().join("scheme_blender_mcp");
        std::fs::create_dir(&extension).expect("extension directory");
        std::fs::write(extension.join("blender_manifest.toml"), "").expect("manifest");
        std::fs::write(directory.path().join("headless_bootstrap.py"), "").expect("bootstrap");
        let default_scene = directory.path().join("default.blend");
        let extra_scene = directory.path().join("extra.blend");
        std::fs::write(&default_scene, "").expect("default scene");
        std::fs::write(&extra_scene, "").expect("extra scene");
        let configuration = directory.path().join("sessions.json");
        std::fs::write(
            &configuration,
            r#"[
            {"name":"game","backend":"headless","blend_file":"extra.blend"},
            {"name":"scratch","backend":"headless"}
        ]"#,
        )
        .expect("sessions file");
        let cli = cli(&[
            "--backend",
            "headless",
            "--extension-dir",
            extension.to_str().expect("extension path"),
            "--blend-file",
            default_scene.to_str().expect("default path"),
            "--sessions-file",
            configuration.to_str().expect("configuration path"),
            "--startup-timeout-secs",
            "42",
            "--blender-log-level",
            "-1",
        ]);
        let sessions = session_configurations(&cli).expect("prepared sessions");
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.name.as_str())
                .collect::<Vec<_>>(),
            ["default", "game", "scratch"]
        );
        for (session, expected_scene) in
            sessions
                .iter()
                .zip([Some(default_scene), Some(extra_scene.clone()), None])
        {
            let SessionBackend::Headless(config) = &session.backend else {
                panic!("headless session expected");
            };
            assert_eq!(config.blend_file, expected_scene);
            assert_eq!(config.extension_dir, extension);
            assert_eq!(config.startup_timeout, Duration::from_secs(42));
            assert_eq!(config.log_level, -1);
        }
        let absolute = vec![SessionDefinition::Headless {
            name: "absolute".to_owned(),
            blend_file: Some(extra_scene.clone()),
        }];
        let prepared =
            prepare_sessions(&cli, absolute, Path::new("unused")).expect("absolute scene");
        let SessionBackend::Headless(config) = &prepared[1].backend else {
            panic!("headless session expected");
        };
        assert_eq!(config.blend_file, Some(extra_scene));
        let missing = vec![SessionDefinition::Headless {
            name: "missing".to_owned(),
            blend_file: Some(PathBuf::from("missing.blend")),
        }];
        assert!(prepare_sessions(&cli, missing, directory.path()).is_err());
    }
}
