//! Tool contract and the built-in coding tools.
//!
//! `Tool` is the second and final dyn seam. The loop preflights calls in
//! source order, executes allowed calls concurrently when every tool is
//! parallel-safe, emits completion events in completion order, and records
//! results in source order.

use crate::cancel::CancelToken;
use crate::model::{ToolCall, ToolResult, ToolResultPart};
use futures::future::{BoxFuture, Either};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub mod edit;
pub mod list;
pub mod read;
pub mod search;
pub mod shell;
pub mod write;

/// Whether a tool can share a batch with other calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Calls may run concurrently, subject to the harness limit.
    Parallel,
    /// The complete batch must run in source order, one call at a time.
    Sequential,
}

/// Failure modes of a tool invocation.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Arguments failed schema or semantic validation.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The operation failed; the message goes back to the model.
    #[error("{0}")]
    Failed(String),
    /// Execution was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// An approval gate denied the operation before it ran.
    #[error("approval denied")]
    ApprovalDenied,
}

/// A value sent to an asynchronous approval gate.
///
/// `cancel` is shared with the tool invocation so a gate can unblock when the
/// run is cancelled or shutting down.
#[derive(Debug, Clone)]
pub struct ApprovalRequest<T> {
    pub value: T,
    pub cancel: CancelToken,
}

/// An asynchronous approval gate's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision<T = ()> {
    Approved(T),
    Denied,
    Cancelled,
}

/// Async approval seam used by [`ApprovalTool`].
///
/// The associated payload lets a gate retain metadata about an approval while
/// the wrapper only needs to distinguish approval, denial, and cancellation.
pub trait ApprovalGate<Request>: Send + Sync {
    type Approved: Send;

    fn approve<'a>(
        &'a self,
        request: ApprovalRequest<Request>,
    ) -> BoxFuture<'a, Result<ApprovalDecision<Self::Approved>, ToolError>>;
}

impl<Request, G> ApprovalGate<Request> for std::sync::Arc<G>
where
    G: ApprovalGate<Request> + ?Sized,
{
    type Approved = G::Approved;

    fn approve<'a>(
        &'a self,
        request: ApprovalRequest<Request>,
    ) -> BoxFuture<'a, Result<ApprovalDecision<Self::Approved>, ToolError>> {
        (**self).approve(request)
    }
}

/// A tool wrapper that requires asynchronous approval before side effects.
pub struct ApprovalTool<T: Tool, G> {
    inner: T,
    gate: G,
}

impl<T: Tool, G> ApprovalTool<T, G> {
    pub fn new(inner: T, gate: G) -> Self {
        ApprovalTool { inner, gate }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn gate(&self) -> &G {
        &self.gate
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T, G> Tool for ApprovalTool<T, G>
where
    T: Tool,
    G: ApprovalGate<ToolCall>,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn spec(&self) -> &ToolSpec {
        self.inner.spec()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.inner.execution_mode()
    }

    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        self.inner.validate(args)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            exec.check_cancelled()?;
            let request = ApprovalRequest {
                value: call.clone(),
                cancel: exec.cancel.clone(),
            };
            let decision = {
                let approval = self.gate.approve(request);
                let cancelled = exec.cancel.cancelled();
                match futures::future::select(approval, cancelled).await {
                    Either::Left((decision, _)) => decision?,
                    Either::Right((_, _)) => return Err(ToolError::Cancelled),
                }
            };

            match decision {
                ApprovalDecision::Approved(_) => {
                    exec.check_cancelled()?;
                    self.inner.execute(call, exec).await
                }
                ApprovalDecision::Denied => Err(ToolError::ApprovalDenied),
                ApprovalDecision::Cancelled => Err(ToolError::Cancelled),
            }
        })
    }
}

/// Result of executing one tool call.
pub struct ExecOutcome {
    /// Content parts returned to the model.
    pub parts: Vec<ToolResultPart>,
    /// Structured payload for UI rendering (opaque to the loop).
    pub details: Option<Value>,
    /// Early-termination hint: the batch stops only when every finalized
    /// result in it sets this.
    pub terminate: bool,
}

impl ExecOutcome {
    pub fn text(t: impl Into<String>) -> Self {
        ExecOutcome {
            parts: vec![ToolResultPart::Text(t.into())],
            details: None,
            terminate: false,
        }
    }
}

/// Filesystem root and policy shared by the built-in tools.
#[derive(Clone)]
pub struct ToolContext {
    /// Working directory; every path resolves against it.
    pub cwd: PathBuf,
    /// Allowed roots for read/write (defaults to `cwd` when empty).
    pub allowed_roots: Vec<PathBuf>,
    /// Whether shell execution is permitted.
    pub allow_shell: bool,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ToolContext {
            cwd: cwd.into(),
            allowed_roots: Vec::new(),
            allow_shell: true,
        }
    }

    /// Resolve and sandbox an existing path against the allowed roots.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.cwd.join(raw)
        };
        let canonical = joined
            .canonicalize()
            .map_err(|e| ToolError::InvalidArguments(format!("{path}: {e}")))?;
        self.sandbox(canonical, path)
    }

    /// Resolve a path that may not exist yet, while checking its deepest
    /// existing ancestor. Parent-directory components are rejected so lexical
    /// normalization cannot turn a safe-looking path into an escape.
    pub fn resolve_for_write(&self, path: &str) -> Result<PathBuf, ToolError> {
        let raw = Path::new(path);
        if raw
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(ToolError::InvalidArguments(format!(
                "{path}: parent traversal is not allowed"
            )));
        }
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.cwd.join(raw)
        };
        if let Ok(canonical) = joined.canonicalize() {
            return self.sandbox(canonical, path);
        }

        let mut probe = joined.as_path();
        let mut suffix = PathBuf::new();
        loop {
            if let Ok(canonical) = probe.canonicalize() {
                return self.sandbox(canonical.join(&suffix), path);
            }
            let Some(parent) = probe.parent() else {
                return Err(ToolError::InvalidArguments(format!(
                    "{path}: cannot resolve"
                )));
            };
            let Some(name) = probe.file_name() else {
                return Err(ToolError::InvalidArguments(format!("{path}: invalid path")));
            };
            suffix = if suffix.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(name).join(suffix)
            };
            probe = parent;
        }
    }

    fn sandbox(&self, canonical: PathBuf, requested: &str) -> Result<PathBuf, ToolError> {
        let roots: Vec<PathBuf> = if self.allowed_roots.is_empty() {
            vec![
                self.cwd
                    .canonicalize()
                    .map_err(|e| ToolError::InvalidArguments(format!("workspace root: {e}")))?,
            ]
        } else {
            self.allowed_roots
                .iter()
                .map(|root| {
                    root.canonicalize()
                        .map_err(|e| ToolError::InvalidArguments(format!("allowed root: {e}")))
                })
                .collect::<Result<_, _>>()?
        };
        if roots.iter().any(|root| canonical.starts_with(root)) {
            Ok(canonical)
        } else {
            Err(ToolError::InvalidArguments(format!(
                "{requested}: outside allowed roots"
            )))
        }
    }
}

/// Execution context handed to every invocation.
pub struct ExecutionContext<'a> {
    /// The one context shared by all built-ins and custom tools in a batch.
    pub ctx: &'a ToolContext,
    pub cancel: crate::CancelToken,
}

impl ExecutionContext<'_> {
    pub fn check_cancelled(&self) -> Result<(), ToolError> {
        if self.cancel.is_cancelled() {
            Err(ToolError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The tool dyn seam.
pub trait Tool: Send + Sync {
    /// Stable name the model calls.
    fn name(&self) -> &'static str;

    /// Advertisement shown to the model. Must be stable for the tool's life.
    fn spec(&self) -> &ToolSpec;

    /// Whether this tool forces its complete batch to execute sequentially.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// Validate arguments without side effects (the loop's preflight stage).
    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        validate_args(args, self.spec())
    }

    /// Execute the call. Cancellation must be honored at every await point and
    /// before each externally visible side effect.
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> futures::future::BoxFuture<'a, Result<ExecOutcome, ToolError>>;
}

/// Static tool advertisement.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (object) for the arguments.
    pub parameters: Value,
    /// Required argument names for default validation.
    pub required: Vec<String>,
}

/// Validate required keys plus JSON Schema types/bounds from `spec.parameters`.
pub fn validate_args(args: &Value, spec: &ToolSpec) -> Result<(), ToolError> {
    check_required(args, &spec.required)?;
    validate_schema(args, &spec.parameters, "")
}

fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<(), ToolError> {
    let Some(kind) = schema.get("type").and_then(Value::as_str) else {
        return Ok(());
    };
    let label = if path.is_empty() {
        "arguments".into()
    } else {
        path.to_owned()
    };
    match kind {
        "object" => {
            let obj = value
                .as_object()
                .ok_or_else(|| ToolError::InvalidArguments(format!("{label} must be an object")))?;
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, child) in properties {
                    if let Some(item) = obj.get(key) {
                        let child_path = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        validate_schema(item, child, &child_path)?;
                    }
                }
            }
        }
        "string" => {
            let text = value
                .as_str()
                .ok_or_else(|| ToolError::InvalidArguments(format!("{label} must be a string")))?;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) < min
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is shorter than {min}"
                )));
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && (text.chars().count() as u64) > max
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is longer than {max}"
                )));
            }
        }
        "integer" => {
            let number = value.as_i64().ok_or_else(|| {
                ToolError::InvalidArguments(format!("{label} must be an integer"))
            })?;
            if let Some(min) = schema.get("minimum").and_then(Value::as_i64)
                && number < min
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is below {min}"
                )));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_i64)
                && number > max
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is above {max}"
                )));
            }
        }
        "number" => {
            let number = value
                .as_f64()
                .ok_or_else(|| ToolError::InvalidArguments(format!("{label} must be a number")))?;
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && number < min
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is below {min}"
                )));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && number > max
            {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} is above {max}"
                )));
            }
        }
        "boolean" => {
            if !value.is_boolean() {
                return Err(ToolError::InvalidArguments(format!(
                    "{label} must be a boolean"
                )));
            }
        }
        "array" => {
            let items = value
                .as_array()
                .ok_or_else(|| ToolError::InvalidArguments(format!("{label} must be an array")))?;
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate_schema(item, item_schema, &format!("{label}[{index}]"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Shared validation: every listed key exists in the object.
pub fn check_required(args: &Value, required: &[String]) -> Result<(), ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::InvalidArguments("expected an object".into()))?;
    for key in required {
        if !obj.contains_key(key) {
            return Err(ToolError::InvalidArguments(format!(
                "missing argument: {key}"
            )));
        }
    }
    Ok(())
}

/// Convert an error into an error tool result (loop helper).
pub fn error_result(call: &ToolCall, err: &ToolError) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![ToolResultPart::Text(err.to_string())],
        is_error: true,
        details: None,
    }
}

/// Convert an outcome into a success tool result (loop helper).
pub fn ok_result(call: &ToolCall, outcome: ExecOutcome) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: outcome.parts,
        is_error: false,
        details: outcome.details.map(std::sync::Arc::new),
    }
}

/// Estimate of the response size for budget checks (bytes).
pub fn result_size(r: &ToolResult) -> u64 {
    r.content
        .iter()
        .map(|p| match p {
            ToolResultPart::Text(t) => t.len() as u64,
            ToolResultPart::Image { data_b64, .. } => (data_b64.len() / 4 * 3) as u64,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_args_enforces_types_and_bounds() {
        let spec = ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 10 }
                }
            }),
            required: vec!["path".into()],
        };
        assert!(validate_args(&json!({"path":"a","limit":3}), &spec).is_ok());
        assert!(validate_args(&json!({"limit":3}), &spec).is_err());
        assert!(validate_args(&json!({"path":1}), &spec).is_err());
        assert!(validate_args(&json!({"path":"a","limit":0}), &spec).is_err());
    }

    #[test]
    fn builtin_spec_is_stable_across_calls() {
        let tool = crate::ReadTool::unbound();
        assert!(std::ptr::eq(tool.spec(), tool.spec()));
        assert_eq!(tool.spec().name, "read");
    }
}
