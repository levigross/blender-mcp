use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use blender_mcp_protocol::{
    BlenderReport, BridgeError, BridgeOperation, BridgeResponse, OperatorCatalog,
    OperatorDescriptor, RnaReference,
};
use blender_mcp_transport::BlenderBridge;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steel::{
    rerrs::{ErrorKind, SteelErr},
    rvals::{FromSteelVal, SteelVal},
    steel_vm::{builtin::BuiltInModule, engine::Engine, register_fn::RegisterFn},
};
use tokio_util::sync::CancellationToken;

use super::marshal::{json_to_steel, steel_to_json};

pub(super) type EvalDeadline = Arc<Mutex<Option<Arc<EvaluationContext>>>>;

pub(super) struct EvaluationContext {
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub in_flight: AtomicBool,
    pub uncertain: AtomicBool,
    pub error: Mutex<Option<BridgeError>>,
    pub bridge_calls: AtomicU64,
    pub bridge_elapsed_ms: AtomicU64,
}

impl EvaluationContext {
    pub(super) fn new(deadline: Instant, cancellation: CancellationToken) -> Self {
        Self {
            deadline,
            cancellation,
            in_flight: AtomicBool::new(false),
            uncertain: AtomicBool::new(false),
            error: Mutex::new(None),
            bridge_calls: AtomicU64::new(0),
            bridge_elapsed_ms: AtomicU64::new(0),
        }
    }

    pub(super) fn potentially_continuing(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) || self.uncertain.load(Ordering::Acquire)
    }
}

/// Cap on summaries returned by `operator-search`, so a broad query stays marshallable.
const MAX_SEARCH_MATCHES: usize = 200;

/// The fields worth scanning before committing to a full `operator-info` lookup.
fn operator_summary(operator: &OperatorDescriptor) -> Value {
    serde_json::json!({
        "idname": operator.idname,
        "steel_name": operator.steel_name,
        "label": operator.label,
        "description": operator.description,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedArtifact {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(skip_serializing)]
    pub data_base64: String,
}

#[derive(Debug)]
pub(super) struct BindingState {
    pub catalog: RwLock<OperatorCatalog>,
    pub pending_catalog: Mutex<Option<OperatorCatalog>>,
    pub reports: Mutex<Vec<BlenderReport>>,
    pub events: Mutex<Vec<Value>>,
    pub artifacts: Mutex<Vec<GeneratedArtifact>>,
    pub catalog_revision: RwLock<String>,
    pub potentially_continuing: AtomicBool,
    pub events_truncated: AtomicBool,
}

impl BindingState {
    pub(super) fn new(catalog: OperatorCatalog) -> Self {
        Self {
            catalog_revision: RwLock::new(catalog.revision.clone()),
            catalog: RwLock::new(catalog),
            pending_catalog: Mutex::new(None),
            reports: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            artifacts: Mutex::new(Vec::new()),
            potentially_continuing: AtomicBool::new(false),
            events_truncated: AtomicBool::new(false),
        }
    }

    pub(super) fn clear_evaluation_output(&self) {
        if let Ok(mut reports) = self.reports.lock() {
            reports.clear();
        }
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
        if let Ok(mut artifacts) = self.artifacts.lock() {
            artifacts.clear();
        }
        self.potentially_continuing.store(false, Ordering::Release);
        self.events_truncated.store(false, Ordering::Release);
    }
}

pub(super) fn register_all(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    register_catalog_functions(engine, bridge, runtime, deadline, state);
    register_operator_functions(engine, bridge, runtime, deadline, state);
    register_rna_functions(engine, bridge, runtime, deadline, state);
    register_custom_property_functions(engine, bridge, runtime, deadline, state);
    register_runtime_functions(engine, bridge, runtime, deadline, state);
    register_workflow_functions(engine, bridge, runtime, deadline, state);
    crate::scheme::math::register_math_functions(engine);
}

fn register_catalog_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    // A real Blender catalogs ~2,500 operators with ~6,200 properties between them, which
    // is far past the marshaller's item budget. These functions therefore return names and
    // compact summaries; `operator-info` returns one full descriptor.
    let catalog_state = Arc::clone(state);
    engine.register_fn("operators", move || -> Result<SteelVal, SteelErr> {
        let catalog = catalog_state
            .catalog
            .read()
            .map_err(|_| steel_error("operator catalog lock is poisoned"))?;
        let idnames = catalog
            .operators
            .iter()
            .map(|operator| Value::String(operator.idname.clone()))
            .collect();
        json_to_steel(&Value::Array(idnames))
    });

    let catalog_state = Arc::clone(state);
    engine.register_fn(
        "operator-search",
        move |query: String| -> Result<SteelVal, SteelErr> {
            let query = query.to_lowercase();
            let catalog = catalog_state
                .catalog
                .read()
                .map_err(|_| steel_error("operator catalog lock is poisoned"))?;
            let hits = catalog
                .operators
                .iter()
                .filter(|operator| {
                    operator.idname.to_lowercase().contains(&query)
                        || operator.label.to_lowercase().contains(&query)
                        || operator.description.to_lowercase().contains(&query)
                })
                .collect::<Vec<_>>();
            let summaries = hits
                .iter()
                .copied()
                .take(MAX_SEARCH_MATCHES)
                .map(operator_summary)
                .collect::<Vec<_>>();
            json_to_steel(&serde_json::json!({
                "total": hits.len(),
                "truncated": hits.len() > summaries.len(),
                "matches": summaries,
            }))
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "operator-info",
        move |idname: String| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::OperatorInfo { idname })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    let refresh_state = Arc::clone(state);
    engine.register_fn("catalog-refresh!", move || -> Result<SteelVal, SteelErr> {
        let value = invoke.call_json(BridgeOperation::CatalogRefresh)?;
        let catalog: OperatorCatalog =
            serde_json::from_value(value).map_err(|error| json_error(&error))?;
        // Returning a summary rather than the catalog keeps the reply inside the
        // marshalling budget. The refreshed aliases install once this evaluation returns.
        let summary = serde_json::json!({
            "revision": catalog.revision,
            "operator_count": catalog.operators.len(),
            "blender_version": catalog.blender_version,
        });
        *refresh_state
            .pending_catalog
            .lock()
            .map_err(|_| steel_error("pending catalog lock is poisoned"))? = Some(catalog);
        json_to_steel(&summary)
    });
}

fn register_operator_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "operator-poll",
        move |idname: String| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::OperatorPoll {
                idname,
                context_override: None,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    register_optional(engine, "op-call", 1, vec![Value::Null], move |arguments| {
        let idname = String::from_steelval(&arguments[0])?;
        let value = normalize_map_argument(&arguments[1])?;
        let (kwargs, execution_context, undo, context_override) = parse_operator_arguments(value)?;
        invoke.call(BridgeOperation::OperatorCall {
            idname,
            kwargs,
            execution_context,
            undo,
            context_override,
        })
    });
}

fn register_custom_property_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "prop-keys",
        move |reference: SteelVal| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::IdPropertyKeys {
                reference: parse_reference(&reference)?,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "prop-get",
        move |reference: SteelVal, key: String| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::IdPropertyGet {
                reference: parse_reference(&reference)?,
                key,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "prop-set!",
        move |reference: SteelVal, key: String, value: SteelVal| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::IdPropertySet {
                reference: parse_reference(&reference)?,
                key,
                value: steel_to_json(&value)?,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "prop-delete!",
        move |reference: SteelVal, key: String| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::IdPropertyDelete {
                reference: parse_reference(&reference)?,
                key,
            })
        },
    );
}

fn register_rna_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn("context-ref", move || -> Result<SteelVal, SteelErr> {
        invoke.call(BridgeOperation::ContextRef)
    });

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn("data-ref", move || -> Result<SteelVal, SteelErr> {
        invoke.call(BridgeOperation::DataRef)
    });

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "rna-get",
        move |reference: SteelVal, attribute: String| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::RnaGet {
                reference: parse_reference(&reference)?,
                attribute,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "rna-set!",
        move |reference: SteelVal,
              attribute: String,
              value: SteelVal|
              -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::RnaSet {
                reference: parse_reference(&reference)?,
                attribute,
                value: steel_to_json(&value)?,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    register_optional(
        engine,
        "rna-call",
        2,
        vec![Value::Array(Vec::new()), Value::Null],
        move |arguments| {
            let reference = &arguments[0];
            let function = String::from_steelval(&arguments[1])?;
            let Value::Array(args) = steel_to_json(&arguments[2])? else {
                return Err(steel_error("rna-call args must be a list or vector"));
            };
            invoke.call(BridgeOperation::RnaCall {
                reference: parse_reference(reference)?,
                function,
                args,
                kwargs: normalize_map_argument(&arguments[3])?,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "rna-describe",
        move |reference: SteelVal| -> Result<SteelVal, SteelErr> {
            invoke.call(BridgeOperation::RnaDescribe {
                reference: parse_reference(&reference)?,
            })
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    register_optional(
        engine,
        "rna-items",
        1,
        vec![Value::from(0), Value::from(100), Value::Null],
        move |arguments| {
            let offset = isize::from_steelval(&arguments[1])?;
            let limit = isize::from_steelval(&arguments[2])?;
            let revision = steel_to_json(&arguments[3])?;
            invoke.call(BridgeOperation::RnaItems {
                reference: parse_reference(&arguments[0])?,
                offset: usize::try_from(offset)
                    .map_err(|_| steel_error("offset must be non-negative"))?,
                limit: usize::try_from(limit)
                    .map_err(|_| steel_error("limit must be non-negative"))?,
                expected_revision: if revision.is_null() || revision == Value::Bool(false) {
                    None
                } else {
                    Some(
                        revision
                            .as_str()
                            .ok_or_else(|| steel_error("expected revision must be a string"))?
                            .to_owned(),
                    )
                },
            })
        },
    );
}

/// Optional arguments share one arity check, including generated operator aliases.
fn register_optional(
    engine: &mut Engine,
    name: &'static str,
    required: usize,
    defaults: Vec<Value>,
    function: impl Fn(&[SteelVal]) -> Result<SteelVal, SteelErr> + Send + Sync + 'static,
) {
    engine.register_value(
        name,
        SteelVal::anonymous_boxed_function(Arc::new(move |args| {
            let maximum = required + defaults.len();
            if !(required..=maximum).contains(&args.len()) {
                return Err(SteelErr::new(
                    ErrorKind::ArityMismatch,
                    format!(
                        "{name} expects {required} to {maximum} arguments, got {}",
                        args.len()
                    ),
                ));
            }
            let mut arguments = args.to_vec();
            for default in &defaults[args.len() - required..] {
                arguments.push(json_to_steel(default)?);
            }
            function(&arguments)
        })),
    );
}

fn register_runtime_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn("blender-status", move || -> Result<SteelVal, SteelErr> {
        invoke.call(BridgeOperation::Status)
    });

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn("scene-summary", move || -> Result<SteelVal, SteelErr> {
        invoke.call(BridgeOperation::SceneSummary)
    });

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn("render!", move || -> Result<SteelVal, SteelErr> {
        let value = invoke.call_json(BridgeOperation::Render {
            filepath: None,
            write_still: true,
        })?;
        invoke.capture_artifact(&value)?;
        json_to_steel(&value)
    });

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "render-to!",
        move |filepath: String| -> Result<SteelVal, SteelErr> {
            let value = invoke.call_json(BridgeOperation::Render {
                filepath: Some(filepath),
                write_still: true,
            })?;
            invoke.capture_artifact(&value)?;
            json_to_steel(&value)
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "render-file!",
        move |filepath: String| -> Result<SteelVal, SteelErr> {
            let value = invoke.call_json(BridgeOperation::Render {
                filepath: Some(filepath),
                write_still: true,
            })?;
            let descriptor = value
                .get("artifact")
                .ok_or_else(|| steel_error("render response omitted its descriptor"))?;
            Ok(SteelVal::StringV(
                required_string(descriptor, "path")?.into(),
            ))
        },
    );

    let invoke = Invocation::new(bridge, runtime, deadline, state);
    engine.register_fn(
        "artifact-get",
        move |artifact_id: String| -> Result<SteelVal, SteelErr> {
            let value = invoke.call_json(BridgeOperation::Artifact {
                artifact_id,
                include_data: true,
            })?;
            invoke.record_artifact(&value)?;
            json_to_steel(&value["artifact"])
        },
    );
}

fn register_workflow_functions(
    engine: &mut Engine,
    bridge: &Arc<dyn BlenderBridge>,
    runtime: &tokio::runtime::Handle,
    deadline: &EvalDeadline,
    state: &Arc<BindingState>,
) {
    let invocation = Invocation::new(bridge, runtime, deadline, state);
    register_artifact_workflow(engine, &invocation);
    register_reference_workflow(engine, &invocation);
    register_batch_workflow(engine, &invocation);
    register_scene_workflow(engine, &invocation);
    register_job_workflow(engine, &invocation);
    register_receipt_workflow(engine, &invocation);
}

fn register_artifact_workflow(engine: &mut Engine, invocation: &Invocation) {
    let invoke = invocation.clone();
    engine.register_fn("artifact-release!", move |ids: SteelVal| {
        let ids = steel_to_json(&ids)?;
        let artifact_ids = match ids {
            Value::String(id) => vec![id],
            Value::Array(ids) => ids
                .into_iter()
                .map(|id| {
                    id.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| steel_error("artifact IDs must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(steel_error(
                    "artifact-release! expects an ID or list of IDs",
                ));
            }
        };
        invoke.call(BridgeOperation::ArtifactRelease { artifact_ids })
    });
    let invoke = invocation.clone();
    register_optional(
        engine,
        "thumbnail!",
        0,
        vec![Value::Null],
        move |arguments| {
            let mut options = normalize_map_argument(&arguments[0])?;
            let filepath = options
                .remove("filepath")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| steel_error("filepath must be a string"))
                })
                .transpose()?;
            let max_size = options
                .remove("max_size")
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|size| (1..=2048).contains(size))
                        .and_then(|size| usize::try_from(size).ok())
                        .ok_or_else(|| steel_error("max_size must be between 1 and 2048"))
                })
                .transpose()?
                .unwrap_or(512);
            if !options.is_empty() {
                return Err(steel_error("unknown thumbnail! options"));
            }
            let value = invoke.call_json(BridgeOperation::Thumbnail { filepath, max_size })?;
            invoke.capture_artifact(&value)?;
            json_to_steel(&value)
        },
    );
}

fn register_reference_workflow(engine: &mut Engine, invocation: &Invocation) {
    for (name, operation) in [
        ("control-status", BridgeOperation::ControlStatus),
        ("reference-stats", BridgeOperation::ReferenceStats),
    ] {
        let invoke = invocation.clone();
        engine.register_fn(name, move || invoke.call(operation.clone()));
    }
    let invoke = invocation.clone();
    engine.register_fn("reference-release!", move |references: SteelVal| {
        let values = match references {
            SteelVal::ListV(values) => values
                .iter()
                .map(parse_reference)
                .collect::<Result<Vec<_>, _>>()?,
            value => vec![parse_reference(&value)?],
        };
        invoke.call(BridgeOperation::ReferenceRelease { references: values })
    });
    let invoke = invocation.clone();
    engine.register_fn(
        "rna-property-info",
        move |reference: SteelVal, attribute: String| {
            invoke.call(BridgeOperation::RnaPropertyInfo {
                reference: parse_reference(&reference)?,
                attribute,
            })
        },
    );
    let invoke = invocation.clone();
    engine.register_fn(
        "rna-function-info",
        move |reference: SteelVal, function: String| {
            invoke.call(BridgeOperation::RnaFunctionInfo {
                reference: parse_reference(&reference)?,
                function,
            })
        },
    );
}

fn register_batch_workflow(engine: &mut Engine, invocation: &Invocation) {
    let invoke = invocation.clone();
    engine.register_fn("batch!", move |commands: SteelVal| {
        let Value::Array(commands) = steel_to_json(&commands)? else {
            return Err(steel_error("batch! expects a list of command hashes"));
        };
        if commands.is_empty() || commands.len() > 100 {
            return Err(steel_error("batch! expects between 1 and 100 commands"));
        }
        let requests = commands
            .into_iter()
            .map(|mut command| {
                if let Some(reference) = command.get_mut("reference")
                    && let Some(inner) = reference.get_mut("$rna_ref")
                {
                    *reference = inner.take();
                }
                let keys = command
                    .as_object()
                    .ok_or_else(|| steel_error("batch commands must be hashes"))?
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let operation: BridgeOperation =
                    serde_json::from_value(command).map_err(|error| json_error(&error))?;
                let canonical =
                    serde_json::to_value(&operation).map_err(|error| json_error(&error))?;
                if keys.iter().any(|key| canonical.get(key).is_none()) {
                    return Err(steel_error("batch command contains unknown fields"));
                }
                if !matches!(
                    operation,
                    BridgeOperation::RnaGet { .. }
                        | BridgeOperation::RnaSet { .. }
                        | BridgeOperation::RnaCall { .. }
                        | BridgeOperation::RnaDescribe { .. }
                        | BridgeOperation::RnaItems { .. }
                        | BridgeOperation::IdPropertyKeys { .. }
                        | BridgeOperation::IdPropertyGet { .. }
                        | BridgeOperation::IdPropertySet { .. }
                        | BridgeOperation::IdPropertyDelete { .. }
                        | BridgeOperation::OperatorCall { .. }
                        | BridgeOperation::ContextRef
                        | BridgeOperation::DataRef
                ) {
                    return Err(steel_error(
                        "batch! only accepts RNA, custom-property, operator, and root-reference commands",
                    ));
                }
                Ok(operation)
            })
            .collect::<Result<Vec<_>, SteelErr>>()?;
        invoke.call(BridgeOperation::Batch {
            requests,
            max_elapsed_ms: 20,
        })
    });
}

fn register_scene_workflow(engine: &mut Engine, invocation: &Invocation) {
    let invoke = invocation.clone();
    register_optional(
        engine,
        "scene-snapshot",
        0,
        vec![Value::from(1000)],
        move |arguments| {
            let limit = usize::try_from(isize::from_steelval(&arguments[0])?)
                .map_err(|_| steel_error("snapshot limit must be positive"))?;
            invoke.call(BridgeOperation::SceneSnapshot { limit })
        },
    );
    let invoke = invocation.clone();
    register_optional(
        engine,
        "scene-diff",
        1,
        vec![Value::Null],
        move |arguments| {
            let after = steel_to_json(&arguments[1])?;
            invoke.call(BridgeOperation::SceneDiff {
                before: steel_to_json(&arguments[0])?,
                after: if after.is_null() { None } else { Some(after) },
            })
        },
    );
    let invoke = invocation.clone();
    engine.register_fn("checkpoint!", move |filepath: String| {
        invoke.call(BridgeOperation::Checkpoint { filepath })
    });
    let invoke = invocation.clone();
    register_optional(
        engine,
        "mesh-from-data!",
        3,
        vec![Value::Null],
        move |arguments| {
            let name = String::from_steelval(&arguments[0])?;
            let vertices = serde_json::from_value(steel_to_json(&arguments[1])?).map_err(|_| {
                steel_error("mesh-from-data! vertices must be a list of (x y z) number lists")
            })?;
            let faces = serde_json::from_value(steel_to_json(&arguments[2])?).map_err(|_| {
                steel_error("mesh-from-data! faces must be a list of vertex-index lists")
            })?;
            let collection = if steel_to_json(&arguments[3])?.is_null() {
                None
            } else {
                Some(parse_reference(&arguments[3])?)
            };
            invoke.call(BridgeOperation::MeshFromData {
                name,
                vertices,
                edges: Vec::new(),
                faces,
                collection,
            })
        },
    );
    let invoke = invocation.clone();
    engine.register_fn("artifact-info", move |artifact_id: String| {
        let value = invoke.call_json(BridgeOperation::Artifact {
            artifact_id,
            include_data: false,
        })?;
        json_to_steel(&value["artifact"])
    });
}

fn register_job_workflow(engine: &mut Engine, invocation: &Invocation) {
    let invoke = invocation.clone();
    register_optional(
        engine,
        "render-start",
        0,
        vec![Value::Null],
        move |arguments| {
            let mut options = normalize_map_argument(&arguments[0])?;
            let filepath = options
                .remove("filepath")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| steel_error("filepath must be a string"))
                })
                .transpose()?;
            let timeout_secs = options
                .remove("timeout_secs")
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|value| (1..=3600).contains(value))
                        .ok_or_else(|| steel_error("job timeout_secs must be between 1 and 3600"))
                })
                .transpose()?;
            if !options.is_empty() {
                return Err(steel_error("unknown render-start options"));
            }
            invoke.call(BridgeOperation::RenderStart {
                filepath,
                timeout_secs,
            })
        },
    );
    for (name, cancel) in [("job-status", false), ("job-cancel", true)] {
        let invoke = invocation.clone();
        engine.register_fn(name, move |job_id: String| {
            invoke.call(if cancel {
                BridgeOperation::JobCancel { job_id }
            } else {
                BridgeOperation::JobStatus { job_id }
            })
        });
    }
    let invoke = invocation.clone();
    register_optional(
        engine,
        "job-result",
        1,
        vec![Value::Bool(true)],
        move |arguments| {
            let job_id = String::from_steelval(&arguments[0])?;
            let attach = bool::from_steelval(&arguments[1])?;
            let value = invoke.call_json(BridgeOperation::JobResult { job_id })?;
            if attach && value.get("artifact").is_some() {
                invoke.capture_artifact(&value)?;
            }
            json_to_steel(&value)
        },
    );
}

fn register_receipt_workflow(engine: &mut Engine, invocation: &Invocation) {
    for (name, result) in [("request-status", false), ("request-result", true)] {
        let invoke = invocation.clone();
        register_optional(engine, name, 1, vec![Value::Null], move |arguments| {
            let request_id = u64::try_from(isize::from_steelval(&arguments[0])?)
                .map_err(|_| steel_error("request id must be non-negative"))?;
            let session = steel_to_json(&arguments[1])?;
            let target_session = if session.is_null() {
                None
            } else {
                Some(
                    session
                        .as_str()
                        .ok_or_else(|| steel_error("target session must be a string"))?
                        .to_owned(),
                )
            };
            invoke.call(if result {
                BridgeOperation::RequestResult {
                    request_id,
                    target_session,
                }
            } else {
                BridgeOperation::RequestStatus {
                    request_id,
                    target_session,
                }
            })
        });
    }
}

#[derive(Clone)]
struct Invocation {
    bridge: Arc<dyn BlenderBridge>,
    runtime: tokio::runtime::Handle,
    deadline: EvalDeadline,
    state: Arc<BindingState>,
}

impl Invocation {
    fn new(
        bridge: &Arc<dyn BlenderBridge>,
        runtime: &tokio::runtime::Handle,
        deadline: &EvalDeadline,
        state: &Arc<BindingState>,
    ) -> Self {
        Self {
            bridge: Arc::clone(bridge),
            runtime: runtime.clone(),
            deadline: Arc::clone(deadline),
            state: Arc::clone(state),
        }
    }

    fn call(&self, operation: BridgeOperation) -> Result<SteelVal, SteelErr> {
        json_to_steel(&self.call_json(operation)?)
    }

    fn call_json(&self, operation: BridgeOperation) -> Result<Value, SteelErr> {
        let context = active_context(&self.deadline)?;
        let timeout = remaining(&context)?;
        let started = Instant::now();
        context.bridge_calls.fetch_add(1, Ordering::Relaxed);
        context
            .in_flight
            .store(operation.is_mutating(), Ordering::Release);
        let response = self.runtime.block_on(self.bridge.request_cancellable(
            operation,
            timeout,
            context.cancellation.clone(),
        ));
        context.bridge_elapsed_ms.fetch_add(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let response = response.map_err(|error| {
            if error.potentially_continuing() {
                context.uncertain.store(true, Ordering::Release);
                self.state
                    .potentially_continuing
                    .store(true, Ordering::Release);
            }
            let (code, retryable) = match &error {
                blender_mcp_transport::TransportError::Blender {
                    code, retryable, ..
                } => (code.clone(), *retryable),
                blender_mcp_transport::TransportError::Timeout { .. } => {
                    ("bridge_timeout".to_owned(), false)
                }
                _ => ("bridge_error".to_owned(), false),
            };
            if let Ok(mut last_error) = context.error.lock() {
                *last_error = Some(BridgeError {
                    code,
                    message: error.to_string(),
                    data: error.structured_data(),
                    retryable,
                    potentially_continuing: error.potentially_continuing(),
                });
            }
            context.in_flight.store(false, Ordering::Release);
            steel_error(error.to_string())
        })?;
        context.in_flight.store(false, Ordering::Release);
        self.record_response(&response)?;
        response
            .result
            .ok_or_else(|| steel_error("Blender response did not contain a result"))
    }

    fn record_response(&self, response: &BridgeResponse) -> Result<(), SteelErr> {
        let mut reports = self
            .state
            .reports
            .lock()
            .map_err(|_| steel_error("report journal lock is poisoned"))?;
        for report in &response.reports {
            if reports.len() >= 128 || report.message.len() > 4096 {
                return Err(steel_error("evaluation report limit exceeded"));
            }
            reports.push(report.clone());
        }
        let mut events = self
            .state
            .events
            .lock()
            .map_err(|_| steel_error("event journal lock is poisoned"))?;
        let mut event_bytes = events
            .iter()
            .map(|event| event.to_string().len())
            .sum::<usize>();
        for event in &response.events {
            event_bytes = event_bytes.saturating_add(event.to_string().len());
            if event.to_string().len() > 16 * 1024 {
                self.state.events_truncated.store(true, Ordering::Release);
                event_bytes = event_bytes.saturating_sub(event.to_string().len());
                continue;
            }
            while !events.is_empty() && (events.len() >= 128 || event_bytes > 256 * 1024) {
                event_bytes = event_bytes.saturating_sub(events.remove(0).to_string().len());
                self.state.events_truncated.store(true, Ordering::Release);
            }
            events.push(event.clone());
        }
        if let Some(revision) = response.catalog_revision.as_ref() {
            self.state
                .catalog_revision
                .write()
                .map_err(|_| steel_error("catalog revision lock is poisoned"))?
                .clone_from(revision);
        }
        Ok(())
    }

    fn capture_artifact(&self, render_value: &Value) -> Result<(), SteelErr> {
        if render_value["artifact"]["inline_available"] == Value::Bool(false) {
            return Ok(());
        }
        let artifact_id = render_value
            .get("artifact")
            .and_then(|artifact| artifact.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| steel_error("render response did not contain an artifact id"))?;
        let artifact = self.call_json(BridgeOperation::Artifact {
            artifact_id: artifact_id.to_owned(),
            include_data: true,
        })?;
        self.record_artifact(&artifact)
    }

    fn record_artifact(&self, value: &Value) -> Result<(), SteelErr> {
        let descriptor = value
            .get("artifact")
            .ok_or_else(|| steel_error("artifact response omitted its descriptor"))?;
        let artifact = GeneratedArtifact {
            id: required_string(descriptor, "id")?,
            name: required_string(descriptor, "name")?,
            mime_type: required_string(descriptor, "mime_type")?,
            data_base64: required_string(value, "data_base64")?,
            metadata: descriptor
                .as_object()
                .ok_or_else(|| steel_error("artifact descriptor must be an object"))?
                .iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "id" | "name" | "mime_type" | "data_base64")
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };
        let mut artifacts = self
            .state
            .artifacts
            .lock()
            .map_err(|_| steel_error("artifact journal lock is poisoned"))?;
        if artifacts.iter().any(|existing| existing.id == artifact.id) {
            return Ok(());
        }
        let total = artifacts
            .iter()
            .map(|artifact| artifact.data_base64.len())
            .sum::<usize>();
        if artifacts.len() >= 16
            || total.saturating_add(artifact.data_base64.len()) > 12 * 1024 * 1024
        {
            return Err(steel_error(
                "evaluation attachment limit exceeded; use artifact-info and retrieve separately",
            ));
        }
        artifacts.push(artifact);
        Ok(())
    }
}

/// Accept either Blender's `$rna_ref` envelope or an already-unwrapped reference.
fn parse_reference(value: &SteelVal) -> Result<RnaReference, SteelErr> {
    let mut json = steel_to_json(value)?;
    if let Some(reference) = json.get_mut("$rna_ref") {
        json = reference.take();
    }
    serde_json::from_value(json).map_err(|_| {
        steel_error(
            "expected an RNA reference; obtain one from (context-ref), (data-ref), rna-get, or rna-items",
        )
    })
}

fn normalize_map_argument(value: &SteelVal) -> Result<BTreeMap<String, Value>, SteelErr> {
    match steel_to_json(value)? {
        Value::Object(values) => Ok(values.into_iter().collect()),
        Value::Array(values) if values.is_empty() => Ok(BTreeMap::new()),
        Value::Null => Ok(BTreeMap::new()),
        _ => Err(steel_error("expected a hash map (or an empty list)")),
    }
}

type OperatorArguments = (
    BTreeMap<String, Value>,
    Option<String>,
    Option<bool>,
    Option<Value>,
);

fn parse_operator_arguments(
    mut values: BTreeMap<String, Value>,
) -> Result<OperatorArguments, SteelErr> {
    if !values.contains_key("kwargs") {
        return Ok((values, None, None, None));
    }
    let kwargs = values
        .remove("kwargs")
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| steel_error("op-call kwargs must be an object"))?
        .into_iter()
        .collect();
    let execution_context = values
        .remove("execution_context")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| steel_error("execution_context must be a string"))
        })
        .transpose()?;
    let undo = values
        .remove("undo")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| steel_error("undo must be a boolean"))
        })
        .transpose()?;
    let context_override = values.remove("context_override");
    if !values.is_empty() {
        return Err(steel_error(format!(
            "unknown op-call option keys: {}",
            values.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok((kwargs, execution_context, undo, context_override))
}

fn required_string(value: &Value, key: &str) -> Result<String, SteelErr> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| steel_error(format!("artifact response omitted {key}")))
}

fn active_context(deadline: &EvalDeadline) -> Result<Arc<EvaluationContext>, SteelErr> {
    let guard = deadline
        .lock()
        .map_err(|_| steel_error("evaluation deadline lock is poisoned"))?;
    guard
        .clone()
        .ok_or_else(|| steel_error("no evaluation deadline is active"))
}

fn remaining(context: &EvaluationContext) -> Result<Duration, SteelErr> {
    if context.cancellation.is_cancelled() {
        return Err(steel_error("scheme_eval cancelled before Blender dispatch"));
    }
    let remaining = context.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(steel_error(
            "scheme_eval deadline expired before Blender dispatch",
        ));
    }
    Ok(remaining)
}

fn json_error(error: &serde_json::Error) -> SteelErr {
    steel_error(format!("JSON conversion failed: {error}"))
}

fn steel_error(message: impl Into<String>) -> SteelErr {
    SteelErr::new(ErrorKind::Generic, message.into())
}

pub(super) fn install_operator_aliases(
    engine: &mut Engine,
    catalog: &OperatorCatalog,
) -> Result<(), SteelErr> {
    let mut source = String::new();
    for operator in &catalog.operators {
        if !valid_alias(&operator.steel_name) {
            return Err(steel_error(format!(
                "catalog supplied invalid Steel alias: {}",
                operator.steel_name
            )));
        }
        let idname = serde_json::to_string(&operator.idname).map_err(|error| json_error(&error))?;
        writeln!(
            source,
            "(define ({} . args) (apply op-call {} args))",
            operator.steel_name, idname
        )
        .map_err(|error| steel_error(format!("failed to generate operator aliases: {error}")))?;
    }
    engine.run(source)?;
    Ok(())
}

fn valid_alias(name: &str) -> bool {
    name.starts_with("bpy/ops/")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'-'))
}

pub(super) fn seal_sandbox(engine: &mut Engine) -> Result<(), SteelErr> {
    let disabled = [
        "load",
        "eval",
        "eval-string",
        "eval-file",
        "command",
        "spawn-process",
        "open-input-file",
        "open-output-file",
        "display",
        "displayln",
        "write",
        "writeln",
        "print",
        "println",
        "read",
        "read-line",
        "get-environment-variable",
        "set-environment-variable!",
    ];
    let mut source = String::new();
    for name in disabled {
        writeln!(
            source,
            "(define ({name} . args) (error \"{name} is disabled in the blender-mcp sandbox\"))"
        )
        .map_err(|error| steel_error(format!("failed to generate sandbox stubs: {error}")))?;
    }
    engine.run(source)?;

    for module in [
        "steel/process",
        "steel/git",
        "steel/meta",
        "steel/fs",
        "steel/ports",
        "steel/io",
        "steel/tcp",
        "steel/http",
        "steel/polling",
        "steel/threads",
    ] {
        engine.register_module(BuiltInModule::new(module));
    }
    Ok(())
}

pub(super) fn validate_source(source: &str) -> Result<(), String> {
    const FORBIDDEN: &[&str] = &[
        "require",
        "require-builtin",
        "load",
        "eval",
        "eval-file",
        "eval-string",
        "include",
    ];
    for token in lexical_tokens(source) {
        if FORBIDDEN.contains(&token.as_str()) {
            return Err(format!("{token} is disabled in the blender-mcp sandbox"));
        }
    }
    Ok(())
}

fn lexical_tokens(source: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == ';' {
            flush_token(&mut tokens, &mut token);
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
        } else if character == '"' {
            flush_token(&mut tokens, &mut token);
            in_string = true;
        } else if character.is_whitespace()
            || matches!(character, '(' | ')' | '[' | ']' | '{' | '}')
        {
            flush_token(&mut tokens, &mut token);
        } else {
            token.push(character);
        }
    }
    flush_token(&mut tokens, &mut token);
    tokens
}

fn flush_token(tokens: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        tokens.push(std::mem::take(token));
    }
}

#[cfg(test)]
mod tests {
    use blender_mcp_protocol::{EnumItem, PropertyDescriptor};

    use super::*;

    #[test]
    fn source_guard_rejects_module_and_file_escape_syntax() {
        for source in [
            "(require-builtin steel/process)",
            "(require \"/etc/passwd\")",
            "(load \"secrets.scm\")",
            "(eval-string \"(+ 1 2)\")",
        ] {
            assert!(validate_source(source).is_err(), "accepted: {source}");
        }
    }

    #[test]
    fn source_guard_ignores_strings_and_comments() {
        validate_source("; require\n(define text \"load eval require-builtin\") (+ 1 2)")
            .expect("words inside comments and strings are inert");
    }

    #[test]
    fn generated_alias_names_are_reversible_and_unique() {
        let catalog = OperatorCatalog {
            protocol_version: 1,
            revision: "test".to_owned(),
            blender_version: "test".to_owned(),
            operators: Vec::new(),
        };
        assert!(catalog.operators.is_empty());
        assert!(valid_alias("bpy/ops/mesh/primitive_cube_add"));
        assert!(!valid_alias("bpy/ops/mesh/(escape)"));
    }

    #[test]
    fn generated_aliases_share_operator_defaults_and_arity_validation() {
        let mut engine = Engine::new_sandboxed();
        register_optional(&mut engine, "op-call", 1, vec![Value::Null], |args| {
            json_to_steel(&serde_json::json!({
                "idname": String::from_steelval(&args[0])?,
                "kwargs": normalize_map_argument(&args[1])?,
            }))
        });
        install_operator_aliases(&mut engine, &realistic_catalog(1)).expect("aliases");
        for (source, kwargs) in [
            ("(bpy/ops/mesh/operator_0)", serde_json::json!({})),
            (
                r#"(bpy/ops/mesh/operator_0 (hash "size" 2))"#,
                serde_json::json!({"size": 2}),
            ),
        ] {
            let values = engine.run(source.to_owned()).expect("alias runs");
            assert_eq!(
                steel_to_json(values.last().unwrap()).unwrap(),
                serde_json::json!({
                    "idname": "mesh.operator_0", "kwargs": kwargs
                })
            );
        }
        let error = engine
            .run("(bpy/ops/mesh/operator_0 (hash) (hash))".to_owned())
            .expect_err("extra argument must not be silently ignored");
        assert!(error.to_string().contains("expects 1 to 2 arguments"));
    }

    /// A catalog the size of a real Blender's, with the property counts to match.
    fn realistic_catalog(operator_count: usize) -> OperatorCatalog {
        let properties = (0..3)
            .map(|index| PropertyDescriptor {
                identifier: format!("property_{index}"),
                kind: "ENUM".to_owned(),
                description: "a property".to_owned(),
                default: None,
                minimum: None,
                maximum: None,
                array_length: None,
                required: false,
                enum_items: (0..4)
                    .map(|item| EnumItem {
                        identifier: format!("ITEM_{item}"),
                        name: format!("Item {item}"),
                        description: "an enum item".to_owned(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        OperatorCatalog {
            protocol_version: 1,
            revision: "realistic".to_owned(),
            blender_version: "5.2.1 LTS".to_owned(),
            operators: (0..operator_count)
                .map(|index| OperatorDescriptor {
                    idname: format!("mesh.operator_{index}"),
                    steel_name: format!("bpy/ops/mesh/operator_{index}"),
                    label: format!("Operator {index}"),
                    description: "does something".to_owned(),
                    module: "mesh".to_owned(),
                    function: format!("operator_{index}"),
                    options: vec!["REGISTER".to_owned(), "UNDO".to_owned()],
                    properties: properties.clone(),
                })
                .collect(),
        }
    }

    /// Regression: a real 2,500-operator catalog carries far more JSON nodes than the
    /// marshaller's item budget, so these must not return whole descriptors.
    #[test]
    fn catalog_queries_stay_within_the_marshalling_budget() {
        let catalog = realistic_catalog(2_500);

        let idnames = catalog
            .operators
            .iter()
            .map(|operator| Value::String(operator.idname.clone()))
            .collect::<Vec<_>>();
        json_to_steel(&Value::Array(idnames)).expect("`operators` must marshal every idname");

        let matches = catalog
            .operators
            .iter()
            .take(MAX_SEARCH_MATCHES)
            .map(operator_summary)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), MAX_SEARCH_MATCHES);
        json_to_steel(&serde_json::json!({
            "total": catalog.operators.len(),
            "truncated": true,
            "matches": matches,
        }))
        .expect("a saturated `operator-search` must marshal");

        json_to_steel(&serde_json::json!({
            "revision": catalog.revision,
            "operator_count": catalog.operators.len(),
            "blender_version": catalog.blender_version,
        }))
        .expect("`catalog-refresh!` must marshal its summary");

        // The shape that used to be returned is exactly what blows the budget.
        let whole = serde_json::to_value(&catalog.operators).expect("catalog serializes");
        assert!(
            json_to_steel(&whole).is_err(),
            "whole descriptors should still be refused, which is why summaries are returned"
        );
    }

    #[test]
    fn rna_references_unwrap_the_blender_envelope() {
        let wrapped = json_to_steel(&serde_json::json!({
            "$rna_ref": {"generation": 3, "id": "rna-7", "type_name": "Object"}
        }))
        .expect("envelope converts");
        let reference = parse_reference(&wrapped).expect("envelope is a reference");
        assert_eq!(reference.generation, 3);
        assert_eq!(reference.id, "rna-7");
        assert_eq!(reference.type_name, "Object");

        // A bare reference, as a caller might reconstruct it, is also accepted.
        let bare = json_to_steel(&serde_json::to_value(&reference).expect("reference serializes"))
            .expect("bare converts");
        assert_eq!(
            parse_reference(&bare).expect("bare is a reference"),
            reference
        );
    }

    #[test]
    fn non_references_explain_where_to_get_one() {
        // `(scene-summary)` and `(operator-info ...)` return plain maps, and passing one
        // to `rna-get` is an easy mistake that must not surface as a serde field error.
        let plain = json_to_steel(&serde_json::json!({"scene": "Scene", "objects": 3}))
            .expect("map converts");
        let message = parse_reference(&plain)
            .expect_err("a plain map is not a reference")
            .to_string();
        assert!(
            message.contains("context-ref") && message.contains("data-ref"),
            "error should name the sources of a reference: {message}"
        );
    }
}
