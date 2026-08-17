//! `edit` tool: exact-match replacement, deliberately refusing ambiguous edits.

use super::{
    ExecOutcome, ExecutionContext, ExecutionMode, Tool, ToolError, ToolSpec, check_required,
};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub struct EditTool;

impl EditTool {
    pub fn unbound() -> Self {
        EditTool
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "edit".into(),
            description: "Replace one exact occurrence in a text file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old": { "type": "string" },
                    "new": { "type": "string" }
                },
                "required": ["path", "old", "new"]
            }),
            required: vec!["path".into(), "old".into(), "new".into()],
        });
        &SPEC
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        check_required(args, &self.spec().required)?;
        for key in ["old", "new"] {
            if !args[key].is_string() {
                return Err(ToolError::InvalidArguments(format!(
                    "{key} must be a string"
                )));
            }
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
            let old = call
                .arguments
                .get("old")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("old must be a string".into()))?;
            let new = call
                .arguments
                .get("new")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("new must be a string".into()))?;
            if old.is_empty() {
                return Err(ToolError::InvalidArguments("old must not be empty".into()));
            }
            let path = exec.ctx.resolve_for_write(path_arg)?;
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
            let count = content.matches(old).count();
            if count == 0 {
                return Err(ToolError::Failed("old text was not found".into()));
            }
            if count > 1 {
                return Err(ToolError::Failed(format!(
                    "old text matched {count} times; provide a unique snippet"
                )));
            }
            let updated = content.replacen(old, new, 1);
            exec.check_cancelled()?;
            std::fs::write(&path, updated)
                .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
            Ok(ExecOutcome {
                parts: vec![ToolResultPart::Text(format!("edited {}", path.display()))],
                details: Some(json!({ "path": path.display().to_string() })),
                terminate: false,
            })
        })
    }
}
