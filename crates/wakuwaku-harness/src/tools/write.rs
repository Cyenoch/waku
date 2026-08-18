//! `write` tool: whole-file replacement with parent-directory creation.

use super::{
    ExecOutcome, ExecutionContext, ExecutionMode, Tool, ToolError, ToolSpec, check_required,
};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub struct WriteTool;

impl WriteTool {
    pub fn unbound() -> Self {
        WriteTool
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "write".into(),
            description: "Create or fully replace a file with the given contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            required: vec!["path".into(), "content".into()],
        });
        &SPEC
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        check_required(args, &self.spec().required)?;
        if !args["content"].is_string() {
            return Err(ToolError::InvalidArguments(
                "content must be a string".into(),
            ));
        }
        Ok(())
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
            let content = call
                .arguments
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("content must be a string".into()))?;
            let path = exec.ctx.resolve_for_write(path_arg)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ToolError::Failed(format!("{}: {e}", parent.display())))?;
            }
            exec.check_cancelled()?;
            std::fs::write(&path, content)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
            Ok(ExecOutcome {
                parts: vec![ToolResultPart::Text(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path.display()
                ))],
                details: Some(
                    json!({ "path": path.display().to_string(), "bytes": content.len() }),
                ),
                terminate: false,
            })
        })
    }
}
