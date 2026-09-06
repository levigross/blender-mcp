//! Bounded, process-wide MCP tasks backed by the SDK's task lifecycle.

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    ErrorData as McpError,
    model::{DetailedTask, Task, UpdateTaskParams},
    task_manager::{TaskExit, TaskManager, TaskOptions},
};
use tokio_util::sync::CancellationToken;

use crate::{
    scheme::SchemeHandle,
    server::{SchemeEvalParams, evaluate_scheme},
};

const CAPACITY: usize = 32;
const RETENTION_MS: u64 = 300_000;

#[derive(Clone, Default)]
pub(crate) struct Tasks(Arc<State>);

#[derive(Default)]
struct State {
    manager: TaskManager,
    // The SDK bounds retention by time, but not count. Keep every admitted ID
    // until the SDK has actually evicted it, including completed task results.
    retained: Mutex<Vec<String>>,
}

impl fmt::Debug for Tasks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Tasks").finish_non_exhaustive()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.manager.shutdown();
    }
}

impl Tasks {
    pub(crate) fn spawn(
        &self,
        worker: SchemeHandle,
        parameters: SchemeEvalParams,
    ) -> Result<Task, McpError> {
        let budget = worker
            .evaluation_timeout(parameters.timeout_secs.map(Duration::from_secs))
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let ttl_ms = u64::try_from(budget.as_millis())
            .ok()
            .and_then(|millis| millis.checked_add(RETENTION_MS))
            .ok_or_else(|| McpError::invalid_params("task timeout is too large", None))?;
        let mut retained = self
            .0
            .retained
            .lock()
            .map_err(|_| McpError::internal_error("task admission unavailable", None))?;
        if retained.len() >= CAPACITY {
            retained.retain(|id| self.0.manager.get_task(id).is_ok());
        }
        if retained.len() >= CAPACITY {
            return Err(McpError::internal_error(
                "task capacity reached; wait for retained tasks to expire",
                Some(serde_json::json!({"code": "task_capacity", "capacity": CAPACITY})),
            ));
        }
        let task = self.0.manager.spawn(
            TaskOptions::new()
                .with_ttl_ms(ttl_ms)
                .with_poll_interval_ms(1_000),
            move |context| {
                Box::pin(async move {
                    // A task survives the originating HTTP request. Only explicit
                    // task cancellation, evaluation deadlines, or shutdown stop it.
                    let cancellation = CancellationToken::new();
                    let evaluation = evaluate_scheme(&worker, parameters, cancellation.clone());
                    tokio::pin!(evaluation);
                    let result = tokio::select! {
                    biased;
                    () = context.cancelled() => {
                        cancellation.cancel();
                        evaluation.await
                    }
                    result = &mut evaluation => result,
                    }?;
                    let cancelled_safely =
                        result.structured_content.as_ref().is_some_and(|value| {
                            value["cancelled"] == true && value["potentially_continuing"] == false
                        });
                    if cancelled_safely {
                        Err(TaskExit::Cancelled)
                    } else {
                        // Preserve receipts when Blender may still be running.
                        // SEP-2663 treats tool-level isError results as completed.
                        Ok(result)
                    }
                })
            },
        );
        retained.push(task.task_id.clone());
        Ok(task)
    }

    pub(crate) fn get(&self, id: &str) -> Result<DetailedTask, McpError> {
        self.0.manager.get_task(id)
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<(), McpError> {
        self.0.manager.cancel_task(id)
    }

    pub(crate) fn update(&self, request: UpdateTaskParams) -> Result<(), McpError> {
        self.0
            .manager
            .update_task(&request.task_id, request.input_responses)
    }
}
