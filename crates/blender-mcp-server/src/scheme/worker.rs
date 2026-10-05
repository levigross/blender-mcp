use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use blender_mcp_protocol::{BlenderReport, BridgeOperation, OperatorCatalog};
use blender_mcp_transport::BlenderBridge;
use serde::Serialize;
use serde_json::Value;
use steel::{
    parser::{ast::ExprKind, parser::Parser},
    rerrs::SteelErr,
    rvals::SteelVal,
    steel_vm::engine::Engine,
};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{
    bindings::{
        BindingState, EvalDeadline, EvaluationContext, GeneratedArtifact, install_operator_aliases,
        register_all, seal_sandbox, validate_source,
    },
    marshal,
};

const REQUEST_CAPACITY: usize = 32;
const WATCHDOG_POLL: Duration = Duration::from_millis(5);

const PRELUDE: &str = r"
(define (op-call/default idname) (op-call idname '()))
(define (rna-call/default reference function)
  (rna-call reference function '() '()))
(define (rna-items/default reference)
  (rna-items reference 0 100))
";

/// Helpers for driving Blender, defined in Scheme because they are compositions of the
/// primitives rather than anything the bridge needs to know about.
const STDLIB: &str = include_str!("stdlib.scm");

#[derive(Debug, Clone, Copy)]
pub struct SchemeSettings {
    pub default_timeout: Duration,
    pub maximum_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemeEvalReply {
    pub display: String,
    pub result: Value,
    pub result_complete: bool,
    pub display_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialization_error: Option<String>,
    pub metrics: Value,
    pub reports: Vec<BlenderReport>,
    pub events: Vec<Value>,
    pub events_truncated: bool,
    pub artifacts: Vec<GeneratedArtifact>,
    pub catalog_revision: String,
}

#[derive(Debug, Clone, Serialize, Error)]
#[error("{message}")]
pub struct SchemeEvalError {
    pub code: String,
    pub message: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub potentially_continuing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl SchemeEvalError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            timed_out: false,
            cancelled: false,
            potentially_continuing: false,
            data: None,
            retryable: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemeWorkerStatus {
    pub state: &'static str,
    pub queued: usize,
}

struct EvalRequest {
    code: String,
    budget: Duration,
    context: Arc<EvaluationContext>,
    _admission: OwnedSemaphorePermit,
    reset: bool,
    include_events: bool,
    cancellation: CancellationToken,
    reply: oneshot::Sender<Result<SchemeEvalReply, SchemeEvalError>>,
}

enum WorkerMessage {
    Evaluate(EvalRequest),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Debug)]
struct SharedStatus {
    state: AtomicU8,
    queued: AtomicUsize,
}

impl SharedStatus {
    const STARTING: u8 = 0;
    const READY: u8 = 1;
    const BUSY: u8 = 2;
    const STOPPED: u8 = 3;

    fn snapshot(&self) -> SchemeWorkerStatus {
        let state = match self.state.load(Ordering::Acquire) {
            Self::STARTING => "starting",
            Self::READY => "ready",
            Self::BUSY => "busy",
            _ => "stopped",
        };
        SchemeWorkerStatus {
            state,
            queued: self.queued.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone)]
pub struct SchemeHandle {
    tx: mpsc::Sender<WorkerMessage>,
    admission: Arc<Semaphore>,
    settings: SchemeSettings,
    status: Arc<SharedStatus>,
    state: Arc<BindingState>,
    bridge: Arc<dyn BlenderBridge>,
}

impl std::fmt::Debug for SchemeHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SchemeHandle")
            .field("status", &self.status.snapshot())
            .finish_non_exhaustive()
    }
}

impl SchemeHandle {
    pub(crate) fn evaluation_timeout(
        &self,
        timeout_override: Option<Duration>,
    ) -> Result<Duration, SchemeEvalError> {
        let budget = timeout_override.unwrap_or(self.settings.default_timeout);
        if budget.is_zero() || budget > self.settings.maximum_timeout {
            return Err(SchemeEvalError::new(
                "invalid_timeout",
                format!(
                    "timeout must be between 1 second and {} seconds",
                    self.settings.maximum_timeout.as_secs()
                ),
            ));
        }
        Ok(budget)
    }

    pub async fn evaluate(
        &self,
        code: String,
        timeout_override: Option<Duration>,
        reset: bool,
        include_events: bool,
        cancellation: CancellationToken,
    ) -> Result<SchemeEvalReply, SchemeEvalError> {
        let budget = self.evaluation_timeout(timeout_override)?;

        let admission = self.admission.clone().try_acquire_owned().map_err(|_| {
            let mut error =
                SchemeEvalError::new("worker_busy", "Scheme request capacity is exhausted");
            error.retryable = Some(true);
            error
        })?;
        let deadline = Instant::now() + budget;
        let cancellation = cancellation.child_token();
        // Dropping the HTTP future must also expire queued work. The permit moves
        // with the request so abandoned queued requests still count toward capacity.
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let context = Arc::new(EvaluationContext::new(deadline, cancellation.clone()));
        if cancellation.is_cancelled() {
            return Err(interrupted_error(&context, false));
        }

        let (reply_tx, mut reply_rx) = oneshot::channel();
        self.status.queued.fetch_add(1, Ordering::AcqRel);
        if self
            .tx
            .try_send(WorkerMessage::Evaluate(EvalRequest {
                code,
                budget,
                context: Arc::clone(&context),
                _admission: admission,
                reset,
                include_events,
                cancellation: cancellation.clone(),
                reply: reply_tx,
            }))
            .is_err()
        {
            self.status.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(SchemeEvalError::new(
                "worker_stopped",
                "Steel worker has stopped",
            ));
        }

        tokio::select! {
            biased;
            reply = &mut reply_rx => reply.map_err(|_| SchemeEvalError::new(
                "worker_stopped",
                "Steel worker dropped the evaluation reply",
            ))?,
            () = cancellation.cancelled() => {
                settle_interruption(&mut reply_rx, &context, false).await
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                cancellation.cancel();
                settle_interruption(&mut reply_rx, &context, true).await
            }
        }
    }

    pub fn status(&self) -> SchemeWorkerStatus {
        self.status.snapshot()
    }

    pub async fn artifact(
        &self,
        artifact_id: String,
        cancellation: CancellationToken,
    ) -> Result<Value, blender_mcp_transport::TransportError> {
        self.bridge
            .request_cancellable(
                BridgeOperation::Artifact {
                    artifact_id,
                    include_data: true,
                },
                Duration::from_secs(10),
                cancellation,
            )
            .await
            .map(|response| response.result.unwrap_or(Value::Null))
    }

    pub fn catalog(&self) -> OperatorCatalog {
        self.state.catalog.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |catalog| catalog.clone(),
        )
    }
}

pub struct SchemeWorker {
    handle: SchemeHandle,
    join: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SchemeWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SchemeWorker")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl SchemeWorker {
    pub async fn spawn(
        bridge: Arc<dyn BlenderBridge>,
        catalog: OperatorCatalog,
        runtime: tokio::runtime::Handle,
        settings: SchemeSettings,
    ) -> Result<Self, SchemeEvalError> {
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        let status = Arc::new(SharedStatus {
            state: AtomicU8::new(SharedStatus::STARTING),
            queued: AtomicUsize::new(0),
        });
        let state = Arc::new(BindingState::new(catalog));
        let thread_status = Arc::clone(&status);
        let thread_state = Arc::clone(&state);
        let handle_bridge = Arc::clone(&bridge);

        let join = std::thread::Builder::new()
            .name("blender-mcp-steel".to_owned())
            // The closure owns these for the thread's lifetime; `worker_main` only borrows them.
            .spawn(move || {
                worker_main(
                    rx,
                    &bridge,
                    &runtime,
                    &thread_state,
                    &thread_status,
                    ready_tx,
                );
            })
            .map_err(|error| SchemeEvalError::new("worker_spawn", error.to_string()))?;

        ready_rx.await.map_err(|_| {
            SchemeEvalError::new("worker_init", "Steel worker exited during startup")
        })??;

        Ok(Self {
            handle: SchemeHandle {
                tx,
                admission: Arc::new(Semaphore::new(REQUEST_CAPACITY)),
                settings,
                status,
                state,
                bridge: handle_bridge,
            },
            join: Some(join),
        })
    }

    pub fn handle(&self) -> SchemeHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) {
        let (done_tx, done_rx) = oneshot::channel();
        drop(self.handle.tx.send(WorkerMessage::Shutdown(done_tx)).await);
        drop(done_rx.await);
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            warn!("Steel worker panicked during shutdown");
        }
    }
}

fn worker_main(
    mut rx: mpsc::Receiver<WorkerMessage>,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    state: &Arc<BindingState>,
    status: &Arc<SharedStatus>,
    ready: oneshot::Sender<Result<(), SchemeEvalError>>,
) {
    let deadline: EvalDeadline = Arc::new(std::sync::Mutex::new(None));
    let initial_catalog = current_catalog(state);
    let mut engine = match build_engine(bridge, runtime, &deadline, state, &initial_catalog) {
        Ok(engine) => engine,
        Err(error) => {
            status.state.store(SharedStatus::STOPPED, Ordering::Release);
            ready
                .send(Err(SchemeEvalError::new("worker_init", error.to_string())))
                .ok();
            return;
        }
    };
    status.state.store(SharedStatus::READY, Ordering::Release);
    ready.send(Ok(())).ok();
    info!("Steel worker started");

    while let Some(message) = rx.blocking_recv() {
        match message {
            WorkerMessage::Evaluate(request) => {
                status.queued.fetch_sub(1, Ordering::AcqRel);
                status.state.store(SharedStatus::BUSY, Ordering::Release);
                process_request(&mut engine, request, bridge, runtime, &deadline, state);
                status.state.store(SharedStatus::READY, Ordering::Release);
            }
            WorkerMessage::Shutdown(done) => {
                done.send(()).ok();
                break;
            }
        }
    }
    status.state.store(SharedStatus::STOPPED, Ordering::Release);
    info!("Steel worker stopped");
}

fn process_request(
    engine: &mut Engine,
    request: EvalRequest,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    if request.cancellation.is_cancelled() || Instant::now() >= request.context.deadline {
        request
            .reply
            .send(Err(interrupted_error(
                &request.context,
                Instant::now() >= request.context.deadline,
            )))
            .ok();
        return;
    }
    if let Err(message) = validate_source(&request.code) {
        request
            .reply
            .send(Err(SchemeEvalError::new("sandbox_violation", message)))
            .ok();
        return;
    }
    if request.reset {
        if let Err(error) = rebuild_engine(engine, bridge, runtime, deadline, state) {
            request
                .reply
                .send(Err(SchemeEvalError::new("reset_failed", error.to_string())))
                .ok();
            return;
        }
        // No Scheme value survives a reset, so neither need the handles it held.
        if let Err(error) = runtime
            .block_on(bridge.request(BridgeOperation::ReferenceReset, Duration::from_secs(5)))
        {
            warn!(%error, "could not release Blender handles after a reset");
        }
    }

    state.clear_evaluation_output();
    if let Ok(mut active_deadline) = deadline.lock() {
        *active_deadline = Some(Arc::clone(&request.context));
    }

    let user_forms = user_form_count(&request.code);
    let defined = top_level_defines(&request.code);
    let run = run_with_watchdog(
        engine,
        request.code,
        request
            .context
            .deadline
            .saturating_duration_since(Instant::now()),
        request.cancellation.clone(),
    );
    if let Ok(mut active_deadline) = deadline.lock() {
        *active_deadline = None;
    }

    let (evaluation, watchdog_reason) = match run {
        Ok(value) => value,
        Err(payload) => {
            let message = panic_message(&payload);
            error!(%message, "Steel VM panicked; rebuilding");
            if let Err(error) = rebuild_engine(engine, bridge, runtime, deadline, state) {
                error!(%error, "failed to rebuild the Steel engine after a panic");
            }
            request
                .reply
                .send(Err(SchemeEvalError::new(
                    "engine_panic",
                    format!("Steel VM panicked and was rebuilt: {message}"),
                )))
                .ok();
            return;
        }
    };

    let evaluation = evaluation.map(|values| user_values(values, user_forms));
    let response = evaluation_response(
        engine,
        evaluation,
        watchdog_reason,
        request.budget,
        request.include_events,
        state,
        &request.context,
    );
    let response = response.map_err(|mut error| {
        if let Some(hint) = redefinition_hint(&error.message, &defined) {
            error.message.push_str(&hint);
        }
        error
    });
    request.reply.send(response).ok();
}

fn evaluation_response(
    engine: &mut Engine,
    evaluation: Result<Vec<SteelVal>, SteelErr>,
    watchdog_reason: WatchdogReason,
    budget: Duration,
    include_events: bool,
    state: &BindingState,
    context: &EvaluationContext,
) -> Result<SchemeEvalReply, SchemeEvalError> {
    // Reply marshalling and catalog updates must not escape the shared worker's
    // recovery boundary. A formatting failure does not require discarding globals.
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        install_pending_catalog(engine, state);
        match watchdog_reason {
            WatchdogReason::Timeout => {
                let mut error = SchemeEvalError::new(
                    "timeout",
                    format!(
                        "scheme_eval timed out after {} seconds",
                        budget.as_secs_f64()
                    ),
                );
                error.timed_out = true;
                enrich_error(&mut error, context);
                Err(error)
            }
            WatchdogReason::Cancelled => {
                let mut error = SchemeEvalError::new("cancelled", "scheme_eval was cancelled");
                error.cancelled = true;
                enrich_error(&mut error, context);
                Err(error)
            }
            WatchdogReason::Completed => evaluation
                .map(|values| evaluation_reply(&values, include_events, state, context))
                .map_err(|error| {
                    let mut result = SchemeEvalError::new("scheme_error", error.to_string());
                    enrich_error(&mut result, context);
                    if let Ok(domain) = context.error.lock()
                        && let Some(domain) = domain.as_ref()
                    {
                        if result.message.contains(&domain.message) {
                            result.code.clone_from(&domain.code);
                        } else {
                            result.data = None;
                            result.retryable = None;
                        }
                    }
                    result
                }),
        }
    }))
    .unwrap_or_else(|payload| {
        let mut error = SchemeEvalError::new(
            "reply_error",
            format!("reply construction failed: {}", panic_message(&payload)),
        );
        enrich_error(&mut error, context);
        Err(error)
    })
}

fn enrich_error(error: &mut SchemeEvalError, context: &EvaluationContext) {
    error.potentially_continuing = context.potentially_continuing();
    if let Ok(domain) = context.error.lock()
        && let Some(domain) = domain.as_ref()
    {
        error.data.clone_from(&domain.data);
        error.retryable = Some(domain.retryable);
    }
}

fn interrupted_error(context: &EvaluationContext, timed_out: bool) -> SchemeEvalError {
    let mut error = if timed_out {
        SchemeEvalError::new("timeout", "scheme_eval deadline expired")
    } else {
        SchemeEvalError::new("cancelled", "scheme_eval was cancelled")
    };
    error.timed_out = timed_out;
    error.cancelled = !timed_out;
    enrich_error(&mut error, context);
    error
}

async fn settle_interruption(
    reply: &mut oneshot::Receiver<Result<SchemeEvalReply, SchemeEvalError>>,
    context: &EvaluationContext,
    timed_out: bool,
) -> Result<SchemeEvalReply, SchemeEvalError> {
    // Allow the transport a short cleanup window to return the request receipt.
    // The execution deadline is already expired; this never grants more run time.
    if let Ok(Ok(result)) = tokio::time::timeout(Duration::from_millis(100), reply).await {
        if let Err(mut error) = result {
            if timed_out {
                "timeout".clone_into(&mut error.code);
                error.timed_out = true;
                error.cancelled = false;
            } else {
                "cancelled".clone_into(&mut error.code);
                error.timed_out = false;
                error.cancelled = true;
            }
            return Err(error);
        }
        return result;
    }
    Err(interrupted_error(context, timed_out))
}

/// Read the active catalog, tolerating a poisoned lock instead of propagating the panic.
fn current_catalog(state: &BindingState) -> OperatorCatalog {
    state.catalog.read().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |catalog| catalog.clone(),
    )
}

/// Transactionally replace `engine`, leaving the old one in place if initialization fails.
fn rebuild_engine(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) -> Result<(), SteelErr> {
    let catalog = current_catalog(state);
    *engine = build_engine(bridge, runtime, deadline, state, &catalog)?;
    Ok(())
}

/// Install aliases for a catalog refreshed during the evaluation that just returned.
///
/// A `catalog-refresh!` callback cannot rebind the Engine's globals while that Engine is
/// still evaluating, so the refreshed catalog waits here until control is back on the worker.
fn install_pending_catalog(engine: &mut Engine, state: &BindingState) {
    let Some(catalog) = state
        .pending_catalog
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
    else {
        return;
    };
    match install_operator_aliases(engine, &catalog) {
        Ok(()) => {
            if let Ok(mut active) = state.catalog.write() {
                active.clone_from(&catalog);
            }
            if let Ok(mut revision) = state.catalog_revision.write() {
                *revision = catalog.revision;
            }
        }
        Err(error) => warn!(%error, "failed to install refreshed operator aliases"),
    }
}

fn evaluation_reply(
    values: &[SteelVal],
    include_events: bool,
    state: &BindingState,
    context: &EvaluationContext,
) -> SchemeEvalReply {
    let reports = state.reports.lock().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |value| value.clone(),
    );
    let events = if include_events {
        state.events.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |value| value.clone(),
        )
    } else {
        Vec::new()
    };
    let artifacts = state.artifacts.lock().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |value| value.clone(),
    );
    let catalog_revision = state.catalog_revision.read().map_or_else(
        |poisoned| poisoned.into_inner().clone(),
        |value| value.clone(),
    );
    let (display, display_truncated) = marshal::display_values(values);
    let (result, serialization_error) = match marshal::values_to_json(values) {
        Ok(result) => (result, None),
        Err(error) => (Value::Null, Some(error.to_string())),
    };
    SchemeEvalReply {
        display,
        result,
        result_complete: serialization_error.is_none(),
        display_truncated,
        serialization_error,
        metrics: serde_json::json!({
            "bridge_calls": context.bridge_calls.load(Ordering::Relaxed),
            "bridge_elapsed_ms": context.bridge_elapsed_ms.load(Ordering::Relaxed),
        }),
        reports,
        events,
        events_truncated: state.events_truncated.load(Ordering::Acquire),
        artifacts,
        catalog_revision,
    }
}

fn build_engine(
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
    catalog: &OperatorCatalog,
) -> Result<Engine, SteelErr> {
    let mut engine = Engine::new_sandboxed();
    seal_sandbox(&mut engine)?;
    register_all(&mut engine, bridge, runtime, deadline, state);
    engine.run(PRELUDE)?;
    engine.run(STDLIB)?;
    install_operator_aliases(&mut engine, catalog)?;
    Ok(engine)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogReason {
    Completed,
    Timeout,
    Cancelled,
}

/// How many values the user's program should produce: one per top-level form, except
/// macro and module forms, which produce none. `None` when the source does not parse.
fn user_form_count(code: &str) -> Option<usize> {
    let forms = Parser::parse(code).ok()?;
    Some(
        forms
            .iter()
            .filter(|form| !matches!(form, ExprKind::Macro(_) | ExprKind::Require(_)))
            .count(),
    )
}

/// Names the program defines at its top level, for explaining compile errors.
fn top_level_defines(code: &str) -> Vec<String> {
    Parser::parse(code)
        .map(|forms| {
            forms
                .iter()
                .filter_map(|form| match form {
                    ExprKind::Define(define) => define
                        .name
                        .atom_identifier()
                        .map(|name| name.resolve().to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Steel gives every `define` a new binding, so a program that reads an existing
/// global and then redefines it is rejected as a forward reference -- an error that
/// does not mention the redefinition or the remedy.
fn redefinition_hint(message: &str, defined: &[String]) -> Option<String> {
    let name = message
        .split("Cannot reference an identifier before its definition: ")
        .nth(1)?
        .split_whitespace()
        .next()?;
    defined.iter().any(|defined| defined == name).then(|| {
        format!(
            ". This evaluation also defines `{name}`, and in Steel a define creates a new \
             binding, so earlier uses in the same evaluation cannot see the existing one. \
             To wrap and replace a global, keep the old value and use set!: \
             (define old-{name} {name}) (set! {name} (lambda ...)). \
             Nothing was evaluated; existing definitions are unchanged."
        )
    })
}

/// Steel's lambda lifting hoists inner lambdas into synthetic top-level defines and
/// prepends them to the program (`lift_all_local_functions`,
/// `lift_pure_local_functions`), so `engine.run` returns an extra `#<void>` for each,
/// ahead of the user's values. Keep the last value per user form so one expression
/// returns one value, as documented.
fn user_values(mut values: Vec<SteelVal>, forms: Option<usize>) -> Vec<SteelVal> {
    if let Some(forms) = forms
        && values.len() > forms
    {
        values.drain(..values.len() - forms);
    }
    values
}

fn run_with_watchdog(
    engine: &mut Engine,
    code: String,
    budget: Duration,
    cancellation: CancellationToken,
) -> std::thread::Result<(Result<Vec<SteelVal>, SteelErr>, WatchdogReason)> {
    let controller = engine.get_thread_state_controller();
    let watchdog_controller = controller.clone();
    let done = Arc::new(AtomicBool::new(false));
    let watchdog_done = Arc::clone(&done);
    let reason = Arc::new(AtomicU8::new(0));
    let watchdog_reason = Arc::clone(&reason);
    let started = Instant::now();
    let watchdog = std::thread::spawn(move || {
        while !watchdog_done.load(Ordering::Acquire) {
            if cancellation.is_cancelled() {
                watchdog_reason.store(2, Ordering::Release);
                watchdog_controller.interrupt();
                return;
            }
            if started.elapsed() >= budget {
                watchdog_reason.store(1, Ordering::Release);
                watchdog_controller.interrupt();
                return;
            }
            std::thread::sleep(WATCHDOG_POLL);
        }
    });

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| engine.run(code)));
    done.store(true, Ordering::Release);
    if watchdog.join().is_err() {
        warn!("Steel watchdog panicked");
    }
    controller.resume();
    let watchdog_reason = match reason.load(Ordering::Acquire) {
        1 => WatchdogReason::Timeout,
        2 => WatchdogReason::Cancelled,
        _ => WatchdogReason::Completed,
    };
    result.map(|evaluation| (evaluation, watchdog_reason))
}

fn panic_message(payload: &dyn std::any::Any) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DelayedBridge {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl BlenderBridge for DelayedBridge {
        async fn request(
            &self,
            _operation: BridgeOperation,
            _timeout: Duration,
        ) -> Result<blender_mcp_protocol::BridgeResponse, blender_mcp_transport::TransportError>
        {
            self.started.notify_one();
            tokio::time::sleep(Duration::from_millis(350)).await;
            Ok(blender_mcp_protocol::BridgeResponse::success(
                1,
                Value::Null,
            ))
        }
        async fn health(&self) -> blender_mcp_transport::BridgeHealth {
            blender_mcp_transport::BridgeHealth {
                mode: blender_mcp_transport::BridgeMode::Live,
                address: "127.0.0.1:1".parse().expect("test address"),
                connected: true,
                process_running: None,
                recent_logs: vec![],
            }
        }
    }

    #[tokio::test]
    async fn expired_queued_source_never_runs_later_and_unicode_reply_preserves_globals() {
        let started = Arc::new(tokio::sync::Notify::new());
        let worker = SchemeWorker::spawn(
            Arc::new(DelayedBridge {
                started: started.clone(),
            }),
            OperatorCatalog {
                protocol_version: blender_mcp_protocol::PROTOCOL_VERSION,
                revision: "test".to_owned(),
                blender_version: "test".to_owned(),
                operators: vec![],
            },
            tokio::runtime::Handle::current(),
            SchemeSettings {
                default_timeout: Duration::from_secs(2),
                maximum_timeout: Duration::from_secs(3),
            },
        )
        .await
        .expect("worker starts");
        let handle = worker.handle();
        let busy_handle = handle.clone();
        let busy = tokio::spawn(async move {
            busy_handle
                .evaluate(
                    "(blender-status)".to_owned(),
                    None,
                    false,
                    false,
                    CancellationToken::new(),
                )
                .await
        });
        started.notified().await;
        let queued = handle
            .evaluate(
                "(define ghost 99)".to_owned(),
                Some(Duration::from_millis(20)),
                false,
                false,
                CancellationToken::new(),
            )
            .await
            .expect_err("queued deadline");
        assert!(queued.timed_out);
        assert!(!queued.potentially_continuing);
        busy.await
            .expect("task joins")
            .expect("first evaluation finishes");
        assert!(
            handle
                .evaluate(
                    "ghost".to_owned(),
                    None,
                    false,
                    false,
                    CancellationToken::new()
                )
                .await
                .is_err(),
            "expired code must never define ghost"
        );
        let source = format!("(define keep 42) \"{}\"", "é".repeat(131_071));
        let reply = handle
            .evaluate(source, None, false, false, CancellationToken::new())
            .await
            .expect("bounded reply");
        assert!(!reply.result_complete);
        assert!(reply.display_truncated);
        assert!(reply.serialization_error.is_some());
        assert_eq!(
            handle
                .evaluate(
                    "keep".to_owned(),
                    None,
                    false,
                    false,
                    CancellationToken::new()
                )
                .await
                .expect("worker remains usable")
                .result,
            Value::from(42)
        );
        worker.shutdown().await;
    }

    #[test]
    fn watchdog_interrupts_cpu_loop_and_engine_recovers() {
        let mut engine = Engine::new_sandboxed();
        let (loop_result, reason) = run_with_watchdog(
            &mut engine,
            "(define (loop) (loop)) (loop)".to_owned(),
            Duration::from_millis(50),
            CancellationToken::new(),
        )
        .expect("VM should not panic");
        assert!(loop_result.is_err());
        assert_eq!(reason, WatchdogReason::Timeout);

        let (recovered, reason) = run_with_watchdog(
            &mut engine,
            "(+ 1 2)".to_owned(),
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .expect("VM should not panic");
        assert_eq!(reason, WatchdogReason::Completed);
        assert_eq!(
            marshal::values_to_display(&recovered.expect("valid eval")),
            "3"
        );
    }

    /// An engine carrying the standard library but no bridge: the RNA primitives are
    /// replaced with stubs so the pure geometry in `stdlib.scm` can be checked without
    /// a running Blender. `rna-get` answers a location by handing back the object,
    /// which the tests pass in as the coordinate list itself.
    fn stdlib_engine() -> Engine {
        stdlib_engine_with_setup("")
    }

    fn stdlib_engine_with_setup(setup: &str) -> Engine {
        let mut engine = Engine::new_sandboxed();
        crate::scheme::math::register_math_functions(&mut engine);
        engine
            .run(
                r#"
                (define captured '())
                ;; A location query hands back the object, which the tests pass in as
                ;; the coordinate list; a rotation query hands back whatever was last
                ;; written, so `facing` can read what `look-at!` just set.
                (define (rna-get object key)
                  (cond [(equal? key "location") object]
                        [(equal? key "rotation_euler") captured]
                        [else '()]))
                (define (rna-set! object key value) (set! captured value) value)
                (define (rna-call reference function args kwargs) '())
                (define (rna-items reference offset limit . revision) (hash "items" '()))
                (define (op-call idname arguments) '())
                (define (context-ref) '())
                (define (data-ref) '())
                (define (render-to! path) '())
                (define (render-file! path) path)
                (define (mesh-from-data! name vertices faces . collection) name)
                (define (batch! commands) commands)
                (define (collection-read collection attribute . page) (hash "total" 0 "stride" 0 "values" '()))
                (define (collection-write! collection attribute offset values) values)
                (define (node-tree! tree spec) spec)
                "#
                .to_owned(),
            )
            .expect("stubs compile");
        engine.run(setup.to_owned()).expect("custom stubs compile");
        engine.run(STDLIB.to_owned()).expect("stdlib compiles");
        engine
    }

    /// Flatten whatever the expression produced into the numbers it contains. Steel
    /// keeps integers exact, so `length` and friends arrive as `IntV` rather than
    /// `NumV` and both have to be accepted.
    fn floats(value: &SteelVal) -> Vec<f64> {
        match value {
            SteelVal::NumV(number) => vec![*number],
            SteelVal::IntV(integer) => {
                vec![f64::from(i32::try_from(*integer).expect("small integer"))]
            }
            SteelVal::ListV(items) => items.iter().flat_map(floats).collect(),
            _ => Vec::new(),
        }
    }

    fn number_list(engine: &mut Engine, source: &str) -> Vec<f64> {
        let values = engine.run(source.to_owned()).expect("runs");
        floats(values.last().expect("a value"))
    }

    #[test]
    fn look_at_aims_along_the_target_direction() {
        // Regression: the Z term was `atan2(dy, dx)`, a quarter turn out, which aimed
        // a camera at nothing. `facing` reconstructs the direction from the angles, so
        // a dot product of 1 against the normalised offset is the whole proof.
        let mut engine = stdlib_engine();
        for (from, target) in [
            ("(list 0.0 -10.0 5.0)", "(list 0.0 0.0 0.0)"),
            ("(list 10.0 0.0 0.0)", "(list 0.0 0.0 0.0)"),
            ("(list -7.0 3.0 9.0)", "(list 1.0 2.0 0.0)"),
        ] {
            let source = format!(
                r"(look-at! {from} {target})
                  (let* ([f (facing captured)]
                         [d (map - {target} {from})]
                         [len (sqrt (apply + (map (lambda (v) (* v v)) d)))])
                    (apply + (map (lambda (a b) (* a (/ b len))) f d)))"
            );
            let dot = number_list(&mut engine, &source);
            let dot = dot.last().copied().unwrap_or_default();
            assert!(
                (dot - 1.0).abs() < 1e-9,
                "{from} -> {target} gave dot {dot}"
            );
        }
    }

    #[test]
    fn ring_places_points_on_the_circle() {
        let mut engine = stdlib_engine();
        // Four points at radius 2 : every one is exactly the radius from the axis.
        let radii = number_list(
            &mut engine,
            "(map (lambda (p) (hypot (list-ref p 0) (list-ref p 1))) (ring 4 2.0 0.0))",
        );
        assert_eq!(radii.len(), 4, "{radii:?}");
        for radius in radii {
            assert!((radius - 2.0).abs() < 1e-9, "{radius}");
        }
    }

    #[test]
    fn chunk_splits_without_losing_or_duplicating_items() {
        // Arguments share the 10,000-item marshalling budget with results, so a large
        // upload has to be split; losing an element here would silently corrupt a mesh.
        let mut engine = stdlib_engine();
        let sizes = number_list(&mut engine, "(map length (chunk (range 0 7) 3))");
        assert_eq!(sizes, vec![3.0, 3.0, 1.0]);
        let flattened = number_list(
            &mut engine,
            "(map exact->inexact (flatten (chunk (range 0 7) 3)))",
        );
        assert_eq!(flattened, (0..7).map(f64::from).collect::<Vec<_>>());
        let empty = number_list(&mut engine, "(length (chunk '() 3))");
        assert_eq!(empty, vec![0.0], "an empty list yields no chunks");
    }

    #[test]
    fn set_rotation_converts_degrees_to_radians() {
        let mut engine = stdlib_engine();
        let angles = number_list(&mut engine, "(set-rotation! '() 90 0 180.0) captured");
        assert_eq!(angles.len(), 3, "{angles:?}");
        assert!((angles[0] - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert!((angles[2] - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn preview_restores_percentage_after_success_and_render_error() {
        let mut engine = stdlib_engine_with_setup(
            r#"
            (define percentage 80)
            (define (rna-get object key) percentage)
            (define (rna-set! object key value) (set! percentage value))
            (define (render-file! path)
              (unless (= percentage 25) (error "preview percentage was not applied"))
              (if (equal? path "fail") (error "render failed") path))
        "#,
        );
        let success = engine
            .run(r#"(preview! "frame.png" 25) percentage"#.to_owned())
            .expect("preview");
        assert_eq!(
            marshal::values_to_json(&success).expect("serializable result"),
            serde_json::json!(["frame.png", 80])
        );
        let error = engine
            .run(r#"(preview! "fail" 25)"#.to_owned())
            .expect_err("render must fail");
        assert!(error.to_string().contains("render failed"));
        assert_eq!(number_list(&mut engine, "percentage"), vec![80.0]);
    }

    #[test]
    fn chunk_rejects_sizes_that_cannot_make_progress() {
        let mut engine = stdlib_engine();
        for size in ["0", "-1", "1/2", "\"two\""] {
            let error = engine
                .run(format!("(chunk '(1 2 3) {size})"))
                .expect_err("invalid size");
            assert!(error.to_string().contains("positive integer"));
        }
    }

    /// What a user sees for `source`: the real engine, then the lifted-define trim.
    fn user_result(engine: &mut Engine, source: &str) -> serde_json::Value {
        let values = engine.run(source.to_owned()).expect("runs");
        marshal::values_to_json(&user_values(values, user_form_count(source)))
            .expect("serializable result")
    }

    #[test]
    fn set_on_a_global_reaches_closures_from_earlier_evaluations() {
        // Each scheme_eval is its own Steel compilation unit. Unpatched steel-core
        // inlines a top-level atom define into closures compiled with it, so a later
        // `set!` is invisible to them (mattwparas/steel#707, pinned in Cargo.toml).
        let mut engine = Engine::new_sandboxed();
        engine
            .run("(define n 0) (define (get-n) n)".to_owned())
            .expect("defines");
        engine.run("(set! n 5)".to_owned()).expect("set!");
        assert_eq!(user_result(&mut engine, "(get-n)"), serde_json::json!(5));
        // Replacing a function with set! (not a second define) reaches its callers.
        engine
            .run("(define (f) 1) (define (g) (f))".to_owned())
            .expect("defines");
        engine
            .run("(set! f (lambda () 2))".to_owned())
            .expect("set!");
        assert_eq!(user_result(&mut engine, "(g)"), serde_json::json!(2));
    }

    #[test]
    fn a_failed_redefinition_keeps_the_existing_global_and_explains_itself() {
        // Regression (mattwparas/steel#713, pinned in Cargo.toml): reading a global and
        // then redefining it in one evaluation fails to compile, and the failed build
        // used to leave the name on an empty slot -- the working definition was lost.
        let mut engine = Engine::new_sandboxed();
        engine
            .run("(define (hand-joints s) (* s 2))".to_owned())
            .expect("defines");
        let wrap = "(define HJ hand-joints) (define (hand-joints s) (+ (HJ s) 1))";
        let error = engine.run(wrap.to_owned()).expect_err("forward reference");
        assert_eq!(
            user_result(&mut engine, "(hand-joints 3)"),
            serde_json::json!(6)
        );

        let hint = redefinition_hint(&error.to_string(), &top_level_defines(wrap))
            .expect("the redefinition is explained");
        assert!(
            hint.contains("set!") && hint.contains("hand-joints"),
            "{hint}"
        );
        assert!(redefinition_hint(&error.to_string(), &[]).is_none());

        // The remedy the hint gives works, in one evaluation.
        engine
            .run("(define HJ hand-joints) (set! hand-joints (lambda (s) (+ (HJ s) 1)))".to_owned())
            .expect("set! replaces the global");
        assert_eq!(
            user_result(&mut engine, "(hand-joints 3)"),
            serde_json::json!(7)
        );
    }

    #[test]
    fn collection_values_pages_in_order_and_mesh_helpers_reshape() {
        // A simulated 2,500-vertex mesh: stride 3, so pages of 1,000 elements.
        let mut engine = stdlib_engine_with_setup(
            r#"
            (define pages-read 0)
            (define written '())
            (define (rna-get object key) key)
            (define (element index) (list index (* 10 index) (* 100 index)))
            (define (collection-read collection attribute offset count)
              (set! pages-read (+ pages-read 1))
              (let* ([total (cond [(equal? collection "vertices") 2500] [(equal? collection "loops") 9] [else 3])]
                     [end (min total (+ offset count))])
                (hash "total" total
                      "stride" (if (equal? attribute "co") 3 1)
                      "values" (cond [(equal? attribute "co")
                                      (apply append (map element (range offset end)))]
                                     [(equal? attribute "loop_start") (list 0 3 6)]
                                     [(equal? attribute "loop_total") (list 3 3 3)]
                                     [else (range offset end)]))))
            (define (collection-write! collection attribute offset values)
              (set! written (append written (list (list offset (length values))))))"#,
        );
        let flat = number_list(&mut engine, "(collection-values \"vertices\" \"co\")");
        assert_eq!(flat.len(), 7500);
        assert_eq!(&flat[7497..], &[2499.0, 24_990.0, 249_900.0]);
        assert_eq!(
            number_list(&mut engine, "pages-read"),
            vec![4.0],
            "a probe and three pages"
        );
        let point = number_list(&mut engine, "(list-ref (mesh-positions \"cube\") 1)");
        assert_eq!(point, vec![1.0, 10.0, 100.0]);
        let writes = last_json(
            &mut engine,
            "(mesh-set-positions! \"cube\" (map element (range 0 2500))) written",
        );
        assert_eq!(
            writes,
            serde_json::json!([[0, 3000], [1000, 3000], [2000, 1500]])
        );
        let faces = last_json(&mut engine, "(mesh-faces \"cube\")");
        assert_eq!(faces, serde_json::json!([[0, 1, 2], [3, 4, 5], [6, 7, 8]]));
    }

    #[test]
    fn expr_to_nodes_compiles_constants_as_defaults_and_folds_variadic_ops() {
        let mut engine = stdlib_engine();
        let graph = last_json(
            &mut engine,
            "(expr->nodes '(let ((s (* p 2))) (+ s (sin q) 1)) (list (list 'p (list \"In\" \"X\")) (list 'q 0.5)) \"e\")",
        );
        // (* p 2), (sin 0.5), (+ s sin) and (+ _ 1): four Math nodes.
        let nodes = graph["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 4, "{graph}");
        assert_eq!(nodes[0]["properties"]["operation"], "MULTIPLY");
        assert_eq!(nodes[0]["inputs"], serde_json::json!([[1, 2]]));
        assert_eq!(nodes[1]["inputs"], serde_json::json!([[0, 0.5]]));
        assert_eq!(nodes[3]["inputs"], serde_json::json!([[1, 1]]));
        assert_eq!(
            graph["links"],
            serde_json::json!([
                ["In", "X", "e1", 0],
                ["e1", 0, "e3", 0],
                ["e2", 0, "e3", 1],
                ["e3", 0, "e4", 0]
            ])
        );
        assert_eq!(graph["output"], serde_json::json!(["e4", 0]));
        let bad = engine
            .run("(expr->nodes '(+ nope 1) '() \"e\")".to_owned())
            .expect_err("unbound");
        assert!(bad.to_string().contains("unbound name"), "{bad}");
    }

    #[test]
    fn lifted_lambdas_do_not_add_values_to_the_result() {
        // Regression: an inner lambda was hoisted into a hidden top-level define, so
        // one expression returned [null, 6] instead of 6.
        let mut engine = Engine::new_sandboxed();
        let lifted = "(let* ([a (list 1 2)] [b (map (lambda (x) (* x 2)) a)]) (apply + b))";
        assert_eq!(
            engine.run(lifted.to_owned()).expect("runs").len(),
            2,
            "Steel still lifts"
        );
        assert_eq!(user_result(&mut engine, lifted), serde_json::json!(6));
        // Several forms still give one value each, in order, with lifting among them.
        assert_eq!(
            user_result(
                &mut engine,
                "(define y 3) (let* ([b (map (lambda (x) (* x y)) (list 1 2))]) b) (+ 1 1)"
            ),
            serde_json::json!([null, [3, 6], 2])
        );
        // Macro definitions produce no value and are not counted.
        assert_eq!(
            user_result(
                &mut engine,
                "(define-syntax twice (syntax-rules () [(_ e) (begin e e)])) (twice (+ 1 2))"
            ),
            serde_json::json!(3)
        );
    }

    /// The last value an expression produced, as JSON. (`values_to_json` unwraps a lone
    /// value, so convert just the last one rather than indexing the whole result.)
    fn last_json(engine: &mut Engine, source: &str) -> serde_json::Value {
        let values = engine.run(source.to_owned()).expect("runs");
        let last = values.last().expect("a value").clone();
        marshal::values_to_json(&[last]).expect("serializable result")
    }

    /// Stubs that log every bridge-facing call as a list, newest last.
    const RECORDING_STUBS: &str = r#"
        (define calls '())
        (define (record! entry) (set! calls (append calls (list entry))))
        (define (rna-call reference function args kwargs)
          (record! (list "call" reference function args)) '())
        (define (rna-set! reference key value) (record! (list "set" reference key value)) value)
        (define (op-call idname arguments) (record! (list "op" idname)) '())
    "#;

    #[test]
    fn matrix_inverse_undoes_rotation_scale_and_translation() {
        // parent! writes this as the parent inverse; an error here silently throws the
        // child across the scene.
        let mut engine = stdlib_engine();
        let round_trip = number_list(
            &mut engine,
            "(define m (list (list 0.0 -2.0 0.0 5.0)
                             (list 2.0 0.0 0.0 -1.0)
                             (list 0.0 0.0 3.0 2.0)
                             (list 0.0 0.0 0.0 1.0)))
             (matrix-apply (matrix-invert-affine m) (matrix-apply m (list 1.5 -2.0 4.0)))",
        );
        for (got, want) in round_trip.iter().zip([1.5, -2.0, 4.0]) {
            assert!((got - want).abs() < 1e-12, "{round_trip:?}");
        }
        let singular = engine
            .run("(matrix-invert-affine (list (list 0 0 0 0) (list 0 1 0 0) (list 0 0 1 0) (list 0 0 0 1)))".to_owned())
            .expect_err("zero scale has no inverse");
        assert!(singular.to_string().contains("singular"));
    }

    #[test]
    fn prism_data_is_closed_and_outward_facing() {
        let mut engine = stdlib_engine();
        let data = last_json(&mut engine, "(prism-data 6 1.0 2.0)");
        let vertices = data[0].as_array().expect("vertices");
        let faces = data[1].as_array().expect("faces");
        assert_eq!((vertices.len(), faces.len()), (12, 8));
        let point = |index: &serde_json::Value| -> [f64; 3] {
            let vertex = &vertices[usize::try_from(index.as_u64().expect("index")).expect("small")];
            [0, 1, 2].map(|axis| vertex[axis].as_f64().expect("coordinate"))
        };
        // Newell's method: the normal of every face must point away from the centre
        // (0, 0, 1), or Blender renders the solid inside out.
        for face in faces {
            let corners: Vec<[f64; 3]> = face.as_array().expect("face").iter().map(point).collect();
            let mut normal = [0.0; 3];
            for (index, a) in corners.iter().enumerate() {
                let b = corners[(index + 1) % corners.len()];
                normal[0] += (a[1] - b[1]) * (a[2] + b[2]);
                normal[1] += (a[2] - b[2]) * (a[0] + b[0]);
                normal[2] += (a[0] - b[0]) * (a[1] + b[1]);
            }
            let count = f64::from(u32::try_from(corners.len()).expect("small face"));
            let centroid =
                [0, 1, 2].map(|axis| corners.iter().map(|c| c[axis]).sum::<f64>() / count);
            let outward =
                normal[0] * centroid[0] + normal[1] * centroid[1] + normal[2] * (centroid[2] - 1.0);
            assert!(outward > 0.0, "face {face} faces inward");
        }
    }

    #[test]
    fn heightfield_data_samples_the_height_function_on_a_centred_grid() {
        let mut engine = stdlib_engine();
        let data = last_json(
            &mut engine,
            "(heightfield-data 2 3 4 (lambda (x y) (+ x y)))",
        );
        let vertices = data[0].as_array().expect("vertices");
        assert_eq!(vertices.len(), 12);
        assert_eq!(data[1].as_array().expect("faces").len(), 6);
        assert_eq!(vertices[0], serde_json::json!([-2.0, -2.0, -4.0]));
        assert_eq!(vertices[11], serde_json::json!([2.0, 2.0, 4.0]));
    }

    #[test]
    fn three_point_rig_stands_at_the_requested_distance() {
        let mut engine = stdlib_engine();
        let distances = number_list(
            &mut engine,
            "(map (lambda (entry)
                    (let ([p (rig-position (list 1.0 2.0 3.0) 5.0 (list-ref entry 1) (list-ref entry 2))])
                      (sqrt (apply + (map (lambda (a b) (* (- a b) (- a b))) p (list 1.0 2.0 3.0))))))
                  three-point-rig)",
        );
        assert_eq!(distances.len(), 3);
        for distance in distances {
            assert!((distance - 5.0).abs() < 1e-9, "{distance}");
        }
    }

    #[test]
    fn select_only_clears_the_old_selection_before_activating() {
        let mut engine = stdlib_engine_with_setup(&format!(
            r#"{RECORDING_STUBS}
            (define (rna-get object key)
              (if (equal? key "selected_objects") (list "a" "b") "handle"))"#
        ));
        let calls = last_json(&mut engine, r#"(select-only! "c") calls"#);
        assert_eq!(
            calls,
            serde_json::json!([
                ["call", "a", "select_set", [false]],
                ["call", "b", "select_set", [false]],
                ["call", "c", "select_set", [true]],
                ["set", "handle", "active", "c"],
            ])
        );
    }

    #[test]
    fn move_to_collection_unlinks_every_owner_then_links() {
        let mut engine = stdlib_engine_with_setup(&format!(
            r#"{RECORDING_STUBS}
            (define (rna-get object key)
              (if (equal? key "users_collection") (list "one" "two") (string-append object "." key)))"#
        ));
        let calls = last_json(&mut engine, r#"(move-to-collection! "cube" "props") calls"#);
        assert_eq!(
            calls,
            serde_json::json!([
                ["call", "one.objects", "unlink", ["cube"]],
                ["call", "two.objects", "unlink", ["cube"]],
                ["call", "props.objects", "link", ["cube"]],
            ])
        );
    }

    #[test]
    fn key_rotation_writes_radians_then_keys_that_frame() {
        let mut engine = stdlib_engine_with_setup(RECORDING_STUBS);
        let calls = last_json(&mut engine, r#"(key-rotation! "cube" 12 0 0 90) calls"#);
        let calls = calls.as_array().expect("calls");
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0][2], "rotation_euler");
        let z = calls[0][3][2].as_f64().expect("radians");
        assert!((z - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
        assert_eq!(
            calls[1],
            serde_json::json!(["call", "cube", "keyframe_insert", ["rotation_euler"]])
        );
    }
}
