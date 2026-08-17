//! `list` tool: bounded, workspace-confined directory listing.

use super::{ExecOutcome, ExecutionContext, Tool, ToolError, ToolSpec, check_required};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub struct ListTool;

impl ListTool {
    pub fn unbound() -> Self {
        ListTool
    }
}

impl Default for ListTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for ListTool {
    fn name(&self) -> &'static str {
        "list"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "list".into(),
            description: "List entries in a workspace directory.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory relative to workspace root" },
                    "recursive": { "type": "boolean" }
                },
                "required": ["path"]
            }),
            required: vec!["path".into()],
        });
        &SPEC
    }

    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        check_required(args, &self.spec().required)?;
        if args.get("recursive").is_some_and(|v| !v.is_boolean()) {
            return Err(ToolError::InvalidArguments(
                "recursive must be a boolean".into(),
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
            let root = exec.ctx.resolve(path_arg)?;
            let recursive = call
                .arguments
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut rows = Vec::new();
            list_dir(&root, recursive, &exec, &mut rows)?;
            rows.sort();
            let truncated = rows.len() > 500;
            rows.truncate(500);
            let out = if rows.is_empty() {
                "(empty)".into()
            } else {
                rows.join("\n")
            };
            Ok(ExecOutcome {
                parts: vec![ToolResultPart::Text(out)],
                details: Some(
                    json!({ "path": root.display().to_string(), "entries": rows.len(), "truncated": truncated }),
                ),
                terminate: false,
            })
        })
    }
}

fn list_dir(
    root: &std::path::Path,
    recursive: bool,
    exec: &ExecutionContext<'_>,
    rows: &mut Vec<String>,
) -> Result<(), ToolError> {
    for item in std::fs::read_dir(root)
        .map_err(|e| ToolError::Failed(format!("{}: {e}", root.display())))?
    {
        exec.check_cancelled()?;
        let item = item.map_err(|e| ToolError::Failed(e.to_string()))?;
        let path = item.path();
        let rel = path.strip_prefix(&exec.ctx.cwd).unwrap_or(&path);
        let suffix = if path.is_dir() { "/" } else { "" };
        rows.push(format!("{}{}", rel.display(), suffix));
        if recursive && path.is_dir() && rows.len() < 500 {
            list_dir(&path, true, exec, rows)?;
        }
    }
    Ok(())
}
