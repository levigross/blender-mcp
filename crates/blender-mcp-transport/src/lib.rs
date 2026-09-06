//! Blender transport implementations.

use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use blender_mcp_protocol::{
    BridgeOperation, BridgeRequest, BridgeResponse, DEFAULT_MAX_FRAME_BYTES, ProtocolError,
    encode_json,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

const LOG_CAPACITY: usize = 512;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Blender bridge at {address} is unavailable: {source}")]
    Unavailable {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("Blender bridge request {request_id} timed out after {timeout:?}")]
    Timeout {
        request_id: u64,
        session_id: String,
        instance_id: Option<String>,
        timeout: Duration,
        potentially_continuing: bool,
    },
    #[error("Blender bridge protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("Blender bridge I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Blender returned {code}: {message}")]
    Blender {
        code: String,
        message: String,
        potentially_continuing: bool,
        data: Option<Box<Value>>,
        retryable: bool,
        request_id: u64,
        session_id: String,
        instance_id: Option<String>,
    },
    #[error("bridge request {request_id} outcome is uncertain: {message}")]
    Uncertain {
        request_id: u64,
        session_id: String,
        instance_id: Option<String>,
        message: String,
        potentially_continuing: bool,
    },
    #[error("Blender bridge compatibility error: {0}")]
    Compatibility(String),
    #[error("failed to start Blender: {0}")]
    Spawn(std::io::Error),
    #[error("Blender exited during startup with {status}; recent output:\n{logs}")]
    EarlyExit { status: String, logs: String },
    #[error("Blender did not expose its bridge within {timeout:?}; recent output:\n{logs}")]
    StartupTimeout { timeout: Duration, logs: String },
    #[error("headless Blender process is not running")]
    NotRunning,
}

impl TransportError {
    pub fn potentially_continuing(&self) -> bool {
        match self {
            Self::Timeout {
                potentially_continuing,
                ..
            }
            | Self::Blender {
                potentially_continuing,
                ..
            }
            | Self::Uncertain {
                potentially_continuing,
                ..
            } => *potentially_continuing,
            _ => false,
        }
    }

    pub fn structured_data(&self) -> Option<Value> {
        match self {
            Self::Blender {
                data,
                request_id,
                session_id,
                instance_id,
                retryable,
                ..
            } => Some(json!({
                "request_id": request_id, "session_id": session_id, "instance_id": instance_id,
                "retryable": retryable, "details": data,
            })),
            Self::Timeout {
                request_id,
                session_id,
                instance_id,
                ..
            }
            | Self::Uncertain {
                request_id,
                session_id,
                instance_id,
                ..
            } => Some(json!({
                "request_id": request_id, "session_id": session_id, "instance_id": instance_id,
            })),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeMode {
    Live,
    Headless,
}

#[derive(Debug, Clone)]
pub struct BridgeHealth {
    pub mode: BridgeMode,
    pub address: SocketAddr,
    pub connected: bool,
    pub process_running: Option<bool>,
    pub recent_logs: Vec<String>,
}

#[async_trait]
pub trait BlenderBridge: Send + Sync + std::fmt::Debug {
    async fn request(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError>;

    async fn health(&self) -> BridgeHealth;

    async fn request_cancellable(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
        _cancellation: CancellationToken,
    ) -> Result<BridgeResponse, TransportError> {
        self.request(operation, request_timeout).await
    }

    async fn shutdown(&self) -> Result<(), TransportError> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct LiveBridge {
    address: SocketAddr,
    connect_timeout: Duration,
    maximum_frame_bytes: usize,
    next_request_id: AtomicU64,
    session_id: String,
    expected_instance: Option<String>,
}

impl LiveBridge {
    pub fn new(address: SocketAddr) -> Self {
        Self {
            address,
            connect_timeout: Duration::from_secs(5),
            maximum_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            next_request_id: AtomicU64::new(1),
            session_id: unique_id(),
            expected_instance: None,
        }
    }

    pub fn with_limits(
        address: SocketAddr,
        connect_timeout: Duration,
        maximum_frame_bytes: usize,
    ) -> Self {
        Self {
            address,
            connect_timeout,
            maximum_frame_bytes,
            next_request_id: AtomicU64::new(1),
            session_id: unique_id(),
            expected_instance: None,
        }
    }

    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    async fn connect(&self) -> Result<TcpStream, TransportError> {
        let stream = timeout(self.connect_timeout, TcpStream::connect(self.address))
            .await
            .map_err(|_| TransportError::Unavailable {
                address: self.address,
                source: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "bridge connection timed out",
                ),
            })?
            .map_err(|source| TransportError::Unavailable {
                address: self.address,
                source,
            })?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    async fn exchange(
        &self,
        request: &BridgeRequest,
        sent: &AtomicBool,
    ) -> Result<BridgeResponse, TransportError> {
        let mut stream = self.connect().await?;
        let encoded = encode_json(request, self.maximum_frame_bytes)?;
        // A partial write may still leave a complete request at the peer.
        sent.store(true, Ordering::Release);
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        let mut prefix = [0_u8; 4];
        stream.read_exact(&mut prefix).await?;
        let length = u32::from_be_bytes(prefix) as usize;
        if length > self.maximum_frame_bytes {
            return Err(ProtocolError::FrameTooLarge {
                actual: length,
                maximum: self.maximum_frame_bytes,
            }
            .into());
        }
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await?;
        let response: BridgeResponse = serde_json::from_slice(&payload)
            .map_err(|error| ProtocolError::MalformedJson(error.to_string()))?;
        response.validate(request.id)?;
        Ok(response)
    }

    fn envelope(&self, operation: BridgeOperation, budget: Duration) -> BridgeRequest {
        let mut request = BridgeRequest::new(
            self.next_request_id.fetch_add(1, Ordering::Relaxed),
            operation,
        );
        request.session_id.clone_from(&self.session_id);
        request.deadline_unix_ms = Some(
            epoch_millis().saturating_add(u64::try_from(budget.as_millis()).unwrap_or(u64::MAX)),
        );
        request
    }

    async fn handshake(&self, budget: Duration) -> Result<Value, TransportError> {
        let request = self.envelope(BridgeOperation::ControlStatus, budget);
        let sent = AtomicBool::new(false);
        let response = timeout(budget, self.exchange(&request, &sent))
            .await
            .map_err(|_| {
                TransportError::Compatibility("control handshake timed out".to_owned())
            })??;
        if let Some(error) = response.error {
            return Err(TransportError::Compatibility(format!(
                "{}: {}",
                error.code, error.message
            )));
        }
        let status = response.result.ok_or_else(|| {
            TransportError::Compatibility("control handshake omitted its result".to_owned())
        })?;
        let instance = status["instance_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                TransportError::Compatibility(
                    "bridge has no instance identity; rebuild/restart the extension".to_owned(),
                )
            })?;
        if self
            .expected_instance
            .as_ref()
            .is_some_and(|expected| expected != instance)
        {
            return Err(TransportError::Compatibility(
                "listener is not the managed Blender child".to_owned(),
            ));
        }
        if status["protocol_version"].as_u64()
            != Some(u64::from(blender_mcp_protocol::PROTOCOL_VERSION))
            || status["native_version"].as_str() != Some(env!("CARGO_PKG_VERSION"))
            || status["extension_version"].as_str() != Some(env!("CARGO_PKG_VERSION"))
            || status["generation"].as_u64().is_none()
            || status["build_id"].as_str().is_none_or(|id| id.len() != 64)
            || !status["capabilities"].as_array().is_some_and(|caps| {
                [
                    "request_receipts",
                    "queued_deadlines",
                    "control_status",
                    "render_jobs",
                    "reference_epochs",
                    "batch",
                ]
                .iter()
                .all(|required| caps.iter().any(|cap| cap.as_str() == Some(*required)))
            })
        {
            return Err(TransportError::Compatibility(
                "extension/native capabilities do not match this server; rebuild/restart both"
                    .to_owned(),
            ));
        }
        Ok(status)
    }

    async fn cancel_best_effort(
        &self,
        request_id: u64,
        instance_id: Option<String>,
    ) -> Option<Value> {
        let cancel_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut cancel = BridgeRequest::new(
            cancel_id,
            BridgeOperation::Cancel {
                request_id,
                target_session: None,
            },
        );
        cancel.session_id.clone_from(&self.session_id);
        cancel.expected_instance = instance_id;
        let sent = AtomicBool::new(false);
        match timeout(Duration::from_millis(50), self.exchange(&cancel, &sent)).await {
            Ok(Ok(response)) => response.result,
            Ok(Err(error)) => {
                warn!(request_id, %error, "bridge cancellation failed");
                None
            }
            Err(_) => {
                warn!(request_id, "bridge cancellation timed out");
                None
            }
        }
    }

    async fn perform(
        &self,
        operation: BridgeOperation,
        budget: Duration,
        cancellation: CancellationToken,
    ) -> Result<BridgeResponse, TransportError> {
        let started = Instant::now();
        let status = tokio::select! {
            result = self.handshake(budget.min(Duration::from_secs(2))) => result?,
            () = cancellation.cancelled() => return Err(TransportError::Compatibility("cancelled before request transmission".to_owned())),
        };
        if cancellation.is_cancelled() {
            return Err(TransportError::Compatibility(
                "cancelled before request transmission".to_owned(),
            ));
        }
        if matches!(operation, BridgeOperation::ControlStatus) {
            return Ok(BridgeResponse::success(0, status));
        }
        let remaining = budget.saturating_sub(started.elapsed());
        let mut request = self.envelope(operation, remaining);
        request.expected_instance = status["instance_id"].as_str().map(str::to_owned);
        request.expected_generation = status["generation"].as_u64();
        let sent = AtomicBool::new(false);
        let exchange = self.exchange(&request, &sent);
        let outcome = tokio::select! {
            result = timeout(remaining, exchange) => Some(result),
            () = cancellation.cancelled() => None,
        };
        match outcome {
            Some(Ok(Ok(response))) => {
                if let Some(error) = response.error.as_ref() {
                    return Err(TransportError::Blender {
                        code: error.code.clone(),
                        message: error.message.clone(),
                        data: error.data.clone().map(Box::new),
                        retryable: error.retryable,
                        potentially_continuing: error.potentially_continuing,
                        request_id: request.id,
                        session_id: self.session_id.clone(),
                        instance_id: request.expected_instance,
                    });
                }
                Ok(response)
            }
            Some(Ok(Err(error))) if !sent.load(Ordering::Acquire) => Err(error),
            other => {
                let transmitted = sent.load(Ordering::Acquire);
                let receipt = if transmitted {
                    self.cancel_best_effort(request.id, request.expected_instance.clone())
                        .await
                } else {
                    None
                };
                let uncertain = transmitted
                    && !receipt.as_ref().is_some_and(|value| {
                        matches!(
                            value["state"].as_str(),
                            Some("cancelled" | "expired" | "succeeded" | "failed")
                        )
                    });
                if matches!(other, Some(Err(_))) {
                    Err(TransportError::Timeout {
                        request_id: request.id,
                        session_id: self.session_id.clone(),
                        instance_id: request.expected_instance,
                        timeout: budget,
                        potentially_continuing: uncertain,
                    })
                } else {
                    let message = match other {
                        Some(Ok(Err(error))) => error.to_string(),
                        _ => "caller cancelled; use the receipt to inspect completed or continuing work".to_owned(),
                    };
                    Err(TransportError::Uncertain {
                        request_id: request.id,
                        session_id: self.session_id.clone(),
                        instance_id: request.expected_instance,
                        message,
                        potentially_continuing: uncertain,
                    })
                }
            }
        }
    }
}

#[async_trait]
impl BlenderBridge for LiveBridge {
    async fn request(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError> {
        self.perform(operation, request_timeout, CancellationToken::new())
            .await
    }

    async fn request_cancellable(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<BridgeResponse, TransportError> {
        self.perform(operation, request_timeout, cancellation).await
    }

    async fn health(&self) -> BridgeHealth {
        let connected = self.handshake(Duration::from_secs(1)).await.is_ok();
        BridgeHealth {
            mode: BridgeMode::Live,
            address: self.address,
            connected,
            process_running: None,
            recent_logs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeadlessConfig {
    pub blender_executable: PathBuf,
    pub bootstrap_script: PathBuf,
    pub extension_dir: PathBuf,
    pub blend_file: Option<PathBuf>,
    pub startup_timeout: Duration,
    pub maximum_frame_bytes: usize,
    /// Blender's `--log-level`. Building the operator catalog resolves every enum
    /// default, which at the default level emits hundreds of `bpy.rna | WARNING`
    /// lines that would crowd real diagnostics out of the bounded log journal.
    /// Python tracebacks and Blender's own errors are unaffected by this.
    pub log_level: i32,
}

#[derive(Debug)]
pub struct HeadlessBridge {
    live: LiveBridge,
    child: Mutex<Option<Child>>,
    logs: Arc<Mutex<VecDeque<String>>>,
    pumps: Mutex<Vec<JoinHandle<()>>>,
}

impl HeadlessBridge {
    pub async fn start(config: HeadlessConfig) -> Result<Self, TransportError> {
        let address = reserve_loopback_address()?;
        let launch_instance = unique_id();
        let mut command = Command::new(&config.blender_executable);
        command
            .arg("--background")
            .arg("--factory-startup")
            .arg("--log-level")
            .arg(config.log_level.to_string());
        if let Some(blend_file) = config.blend_file.as_ref() {
            command.arg(blend_file);
        }
        command
            .arg("--python-exit-code")
            .arg("1")
            .arg("--python")
            .arg(&config.bootstrap_script)
            .arg("--")
            .arg("--host")
            .arg(address.ip().to_string())
            .arg("--port")
            .arg(address.port().to_string())
            .env("BLENDER_MCP_EXTENSION_DIR", &config.extension_dir)
            .env("BLENDER_MCP_INSTANCE_ID", &launch_instance)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(TransportError::Spawn)?;
        let logs = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)));
        let mut pumps = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            pumps.push(tokio::spawn(pump_logs("stdout", stdout, Arc::clone(&logs))));
        }
        if let Some(stderr) = child.stderr.take() {
            pumps.push(tokio::spawn(pump_logs("stderr", stderr, Arc::clone(&logs))));
        }

        let mut live = LiveBridge::with_limits(
            address,
            Duration::from_millis(300),
            config.maximum_frame_bytes,
        );
        live.expected_instance = Some(launch_instance);
        let bridge = Self {
            live,
            child: Mutex::new(Some(child)),
            logs,
            pumps: Mutex::new(pumps),
        };
        if let Err(error) = bridge.wait_until_ready(config.startup_timeout).await {
            if let Some(mut child) = bridge.child.lock().await.take() {
                drop(child.kill().await);
                drop(child.wait().await);
            }
            for pump in bridge.pumps.lock().await.drain(..) {
                pump.abort();
            }
            return Err(error);
        }
        Ok(bridge)
    }

    async fn wait_until_ready(&self, startup_timeout: Duration) -> Result<(), TransportError> {
        let started = Instant::now();
        loop {
            {
                let mut child_guard = self.child.lock().await;
                let child = child_guard.as_mut().ok_or(TransportError::NotRunning)?;
                if let Some(status) = child.try_wait()? {
                    return Err(TransportError::EarlyExit {
                        status: status.to_string(),
                        logs: self.logs_text().await,
                    });
                }
            }
            let readiness = self.live.handshake(Duration::from_millis(500)).await;
            if readiness.is_ok() {
                let mut guard = self.child.lock().await;
                let child = guard.as_mut().ok_or(TransportError::NotRunning)?;
                if let Some(status) = child.try_wait()? {
                    return Err(TransportError::EarlyExit {
                        status: status.to_string(),
                        logs: self.logs_text().await,
                    });
                }
                return Ok(());
            }
            if started.elapsed() >= startup_timeout {
                return Err(TransportError::StartupTimeout {
                    timeout: startup_timeout,
                    logs: format!(
                        "{}\nlast readiness error: {}",
                        self.logs_text().await,
                        readiness.expect_err("successful readiness returned above"),
                    ),
                });
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn logs_text(&self) -> String {
        self.logs
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl BlenderBridge for HeadlessBridge {
    async fn request(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
    ) -> Result<BridgeResponse, TransportError> {
        self.live.request(operation, request_timeout).await
    }

    async fn request_cancellable(
        &self,
        operation: BridgeOperation,
        request_timeout: Duration,
        cancellation: CancellationToken,
    ) -> Result<BridgeResponse, TransportError> {
        self.live
            .request_cancellable(operation, request_timeout, cancellation)
            .await
    }

    async fn health(&self) -> BridgeHealth {
        let running = {
            let mut guard = self.child.lock().await;
            match guard.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(None)),
                None => false,
            }
        };
        let mut health = self.live.health().await;
        health.mode = BridgeMode::Headless;
        health.process_running = Some(running);
        health.recent_logs = self.logs.lock().await.iter().cloned().collect();
        health
    }

    async fn shutdown(&self) -> Result<(), TransportError> {
        drop(
            self.live
                .request(BridgeOperation::Shutdown, Duration::from_secs(2))
                .await,
        );
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            if let Ok(result) = timeout(Duration::from_secs(3), child.wait()).await {
                result?;
            } else {
                child.kill().await?;
                child.wait().await?;
            }
        }
        for pump in self.pumps.lock().await.drain(..) {
            pump.abort();
        }
        Ok(())
    }
}

impl Drop for HeadlessBridge {
    fn drop(&mut self) {
        if let Ok(mut child_guard) = self.child.try_lock()
            && let Some(child) = child_guard.as_mut()
        {
            drop(child.start_kill());
        }
        if let Ok(mut pumps) = self.pumps.try_lock() {
            for pump in pumps.drain(..) {
                pump.abort();
            }
        }
    }
}

fn reserve_loopback_address() -> Result<SocketAddr, TransportError> {
    let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        address.port(),
    ))
}

fn epoch_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn unique_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

async fn pump_logs<R>(label: &'static str, reader: R, logs: Arc<Mutex<VecDeque<String>>>)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut guard = logs.lock().await;
        if guard.len() == LOG_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(format!("{label}: {line}"));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use blender_mcp_protocol::{BridgeRequest, BridgeResponse};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    fn control_result() -> Value {
        json!({"instance_id": "test-instance", "generation": 7,
            "protocol_version": blender_mcp_protocol::PROTOCOL_VERSION,
            "native_version": env!("CARGO_PKG_VERSION"), "extension_version": env!("CARGO_PKG_VERSION"),
            "build_id": "0".repeat(64),
            "capabilities": ["request_receipts", "queued_deadlines", "control_status", "render_jobs", "reference_epochs", "batch"]})
    }

    async fn read_request(stream: &mut TcpStream) -> BridgeRequest {
        let mut prefix = [0_u8; 4];
        stream
            .read_exact(&mut prefix)
            .await
            .expect("request prefix");
        let mut payload = vec![0_u8; u32::from_be_bytes(prefix) as usize];
        stream
            .read_exact(&mut payload)
            .await
            .expect("request payload");
        serde_json::from_slice(&payload).expect("request JSON")
    }

    #[tokio::test]
    async fn correlates_response_ids() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut stream).await;
            let response = BridgeResponse::success(request.id + 1, json!(null));
            stream
                .write_all(&encode_json(&response, 1024).expect("response frame"))
                .await
                .expect("response write");
        });
        let bridge = LiveBridge::new(address);
        assert!(matches!(
            bridge
                .request(BridgeOperation::Status, Duration::from_secs(1))
                .await,
            Err(TransportError::Protocol(
                ProtocolError::CorrelationMismatch { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn reconnects_for_each_request() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&accepted);
        tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                server_count.fetch_add(1, Ordering::Relaxed);
                let request = read_request(&mut stream).await;
                let result = if matches!(request.operation, BridgeOperation::ControlStatus) {
                    control_result()
                } else {
                    assert_eq!(request.expected_instance.as_deref(), Some("test-instance"));
                    assert_eq!(request.expected_generation, Some(7));
                    assert!(!request.session_id.is_empty());
                    assert!(request.deadline_unix_ms.is_some());
                    json!({"ok": true})
                };
                let response = BridgeResponse::success(request.id, result);
                stream
                    .write_all(&encode_json(&response, 1024).expect("response frame"))
                    .await
                    .expect("response write");
            }
        });
        let bridge = LiveBridge::new(address);
        for _ in 0..2 {
            bridge
                .request(BridgeOperation::Status, Duration::from_secs(1))
                .await
                .expect("bridge request");
        }
        assert_eq!(accepted.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn malformed_handshake_is_not_healthy() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut stream).await;
            stream
                .write_all(
                    &encode_json(
                        &BridgeResponse::success(request.id, json!({"ok": true})),
                        1024,
                    )
                    .expect("frame"),
                )
                .await
                .expect("write");
        });
        assert!(!LiveBridge::new(address).health().await.connected);
    }

    #[tokio::test]
    async fn cancelled_queued_request_retains_receipt_without_claiming_continuation() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let peer = tokio::spawn(async move {
            let (mut handshake, _) = listener.accept().await.expect("handshake");
            let request = read_request(&mut handshake).await;
            handshake
                .write_all(
                    &encode_json(&BridgeResponse::success(request.id, control_result()), 4096)
                        .expect("frame"),
                )
                .await
                .expect("write");
            let (mut work, _) = listener.accept().await.expect("work");
            let request = read_request(&mut work).await;
            trigger.cancel();
            let (mut control, _) = listener.accept().await.expect("cancel");
            let cancel = read_request(&mut control).await;
            assert_eq!(cancel.session_id, request.session_id);
            assert!(
                matches!(cancel.operation, BridgeOperation::Cancel { request_id, .. } if request_id == request.id)
            );
            control
                .write_all(
                    &encode_json(
                        &BridgeResponse::success(cancel.id, json!({"state": "cancelled"})),
                        4096,
                    )
                    .expect("frame"),
                )
                .await
                .expect("write");
        });
        let error = LiveBridge::new(address)
            .request_cancellable(
                BridgeOperation::Render {
                    filepath: None,
                    write_still: true,
                },
                Duration::from_secs(2),
                cancellation,
            )
            .await
            .expect_err("cancelled");
        assert!(!error.potentially_continuing());
        let details = error.structured_data().expect("receipt");
        assert_eq!(details["instance_id"], "test-instance");
        assert!(details["request_id"].is_u64());
        peer.await.expect("peer");
    }

    #[tokio::test]
    async fn lost_mutation_response_reports_uncertain_running_work() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let peer = tokio::spawn(async move {
            let (mut handshake, _) = listener.accept().await.expect("handshake");
            let request = read_request(&mut handshake).await;
            handshake
                .write_all(
                    &encode_json(&BridgeResponse::success(request.id, control_result()), 4096)
                        .expect("frame"),
                )
                .await
                .expect("write");
            let (mut work, _) = listener.accept().await.expect("work");
            read_request(&mut work).await;
            drop(work);
            let (mut control, _) = listener.accept().await.expect("cancel");
            let cancel = read_request(&mut control).await;
            control
                .write_all(
                    &encode_json(
                        &BridgeResponse::success(
                            cancel.id,
                            json!({"state": "running", "cancel_requested": true}),
                        ),
                        4096,
                    )
                    .expect("frame"),
                )
                .await
                .expect("write");
        });
        let error = LiveBridge::new(address)
            .request(
                BridgeOperation::Render {
                    filepath: None,
                    write_still: true,
                },
                Duration::from_secs(2),
            )
            .await
            .expect_err("lost response");
        assert!(matches!(error, TransportError::Uncertain { .. }));
        assert!(error.potentially_continuing());
        peer.await.expect("peer");
    }
}
