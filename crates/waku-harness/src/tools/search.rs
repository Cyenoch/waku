//! `search` tool: recursive literal/regex-free text search.

use super::{ExecOutcome, ExecutionContext, Tool, ToolError, ToolSpec};
use crate::model::{ToolCall, ToolResultPart};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub struct SearchTool {
    max_file_bytes: u64,
}

impl SearchTool {
    pub fn unbound() -> Self {
        SearchTool {
            max_file_bytes: 512 * 1024,
        }
    }
    pub fn with_max_file_bytes(mut self, max: u64) -> Self {
        self.max_file_bytes = max;
        self
    }
}

impl Default for SearchTool {
    fn default() -> Self {
        Self::unbound()
    }
}

impl Tool for SearchTool {
    fn name(&self) -> &'static str {
        "search"
    }

    fn spec(&self) -> &ToolSpec {
        static SPEC: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
            name: "search".into(),
            description: "Find a literal string in workspace files.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "pattern": { "type": "string", "description": "Legacy spelling for query" },
                    "path": { "type": "string" },
                    "max_results": { "type": "integer" }
                },
                "required": ["query"]
            }),
            required: vec!["query".into()],
        });
        &SPEC
    }

    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        if !args.get("query").is_some_and(Value::is_string)
            && !args.get("pattern").is_some_and(Value::is_string)
        {
            return Err(ToolError::InvalidArguments("query must be a string".into()));
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
            let query = call
                .arguments
                .get("query")
                .and_then(Value::as_str)
                .or_else(|| call.arguments.get("pattern").and_then(Value::as_str))
                .unwrap_or_default();
            if query.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "query must not be empty".into(),
                ));
            }
            let path_arg = call
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".");
            let root = exec.ctx.resolve(path_arg)?;
            let max_results = call
                .arguments
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .min(500) as usize;
            let mut hits = Vec::new();
            visit(
                &root,
                query,
                max_results,
                self.max_file_bytes,
                &exec,
                &mut hits,
            )?;
            let out = if hits.is_empty() {
                "(no matches)".into()
            } else {
                hits.join("\n")
            };
            Ok(ExecOutcome {
                parts: vec![ToolResultPart::Text(out)],
                details: Some(json!({ "query": query, "matches": hits.len() })),
                terminate: false,
            })
        })
    }
}

fn visit(
    path: &std::path::Path,
    query: &str,
    max_results: usize,
    max_file_bytes: u64,
    exec: &ExecutionContext<'_>,
    hits: &mut Vec<String>,
) -> Result<(), ToolError> {
    if hits.len() >= max_results {
        return Ok(());
    }
    exec.check_cancelled()?;
    let metadata = std::fs::metadata(path)
        .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| ToolError::Failed(e.to_string()))? {
            exec.check_cancelled()?;
            visit(
                &entry.map_err(|e| ToolError::Failed(e.to_string()))?.path(),
                query,
                max_results,
                max_file_bytes,
                exec,
                hits,
            )?;
            if hits.len() >= max_results {
                break;
            }
        }
        return Ok(());
    }
    if metadata.len() > max_file_bytes {
        return Ok(());
    }
    exec.check_cancelled()?;
    let bytes = std::fs::read(path).map_err(|e| ToolError::Failed(e.to_string()))?;
    let Ok(content) = std::str::from_utf8(&bytes) else {
        return Ok(());
    };
    for (line_no, line) in content.lines().enumerate() {
        exec.check_cancelled()?;
        if line.contains(query) {
            let rel = path.strip_prefix(&exec.ctx.cwd).unwrap_or(path);
            hits.push(format!("{}:{}:{}", rel.display(), line_no + 1, line.trim()));
            if hits.len() >= max_results {
                break;
            }
        }
    }
    Ok(())
}
