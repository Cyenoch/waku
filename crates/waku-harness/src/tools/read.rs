//! `read` tool: file contents with offset/limit windowing.

use super::{ExecOutcome, ExecutionContext, Tool, ToolError, ToolSpec};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub struct ReadTool {
    max_bytes: u64,
}

const DEFAULT_MAX_BYTES: u64 = 256 * 1024;

impl ReadTool {
    pub fn unbound() -> Self {
        ReadTool {
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = max;
        self
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "read".into(),
            description: "Read a text file's contents. Returns numbered lines.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the workspace root" },
                    "offset": { "type": "integer", "description": "1-based line to start from" },
                    "limit": { "type": "integer", "description": "Maximum lines to return" }
                },
                "required": ["path"]
            }),
            required: vec!["path".into()],
        });
        &SPEC
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        exec: ExecutionContext<'a>,
    ) -> futures::future::BoxFuture<'a, Result<ExecOutcome, ToolError>> {
        Box::pin(async move {
            exec.check_cancelled()?;
            let path_arg = call
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("path must be a string".into()))?;
            let path = exec.ctx.resolve(path_arg)?;
            let offset = call
                .arguments
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1);
            let limit = call.arguments.get("limit").and_then(Value::as_u64);
            let meta = std::fs::metadata(&path)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
            if meta.len() > self.max_bytes {
                return Err(ToolError::Failed(format!(
                    "file is {} bytes; exceeds read limit {}",
                    meta.len(),
                    self.max_bytes
                )));
            }
            exec.check_cancelled()?;
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
            let lines: Vec<&str> = content.lines().collect();
            let start = (offset as usize).saturating_sub(1);
            let end = limit
                .map(|l| start.saturating_add(l as usize))
                .unwrap_or(lines.len());
            let window = lines
                .iter()
                .enumerate()
                .skip(start)
                .take(end.saturating_sub(start))
                .map(|(i, l)| format!("{:>6}\t{}", i + 1, l))
                .collect::<Vec<_>>()
                .join("\n");
            let out = if window.is_empty() {
                format!("(no lines in range {}..)", offset)
            } else {
                window
            };
            Ok(ExecOutcome {
                parts: vec![ToolResultPart::Text(out)],
                details: Some(json!({ "path": path.display().to_string(), "lines": lines.len() })),
                terminate: false,
            })
        })
    }
}
