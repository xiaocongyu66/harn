//! Glob 工具：基于 harn-search 的 bfs 语义文件发现。

use std::path::PathBuf;

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use harn_search::{GlobOptions, find_glob_sync};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (for example `**/*.rs`). Returns paths sorted by modification \
         time. Respects hidden files by default; does not apply .gitignore."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. `**/*.rs` or `src/**/*.ts`."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search; defaults to the workspace root."
                },
                "type": {
                    "type": "string",
                    "description": "Optional file type filter (rg --type), e.g. rust or ts."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results (default 100, max 1000)."
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip the first N results."
                }
            }),
            &["pattern"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.glob.pattern_required")))?;
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map(|raw| ctx.resolve_path(raw))
            .unwrap_or_else(|| ctx.cwd.clone());
        if !root.is_dir() {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.glob.path_not_dir",
                path = root.display()
            )));
        }
        let max_results = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 1_000) as u32;
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32;
        let type_filter = input
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);

        let options = GlobOptions {
            pattern: Some(pattern.to_string()),
            path: Some(root.to_string_lossy().into_owned()),
            r#type: type_filter,
            hidden: Some(true),
            gitignore: Some(false),
            sort_by_mtime: Some(true),
            max_results: Some(max_results),
            offset: Some(offset),
            timeout_ms: Some(20_000),
            bfs_name: None,
            bfs_icase: None,
            min_depth: None,
            max_depth: None,
            follow: None,
            order: None,
            cancel_key: Some(ctx.agent_id.clone()),
        };

        let result = tokio::task::spawn_blocking(move || find_glob_sync(options))
            .await
            .map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.glob.task_failed", e = e)))?;
        let paths = match result {
            Ok(paths) => paths,
            Err(error) => {
                return Ok(ToolOutput::error(harn_i18n::tr!(
                    "tool.glob.failed",
                    error = error
                )));
            }
        };
        if paths.is_empty() {
            return Ok(ToolOutput::ok("(no matching files)"));
        }
        let cwd = ctx.cwd.clone();
        let rendered: Vec<String> = paths
            .iter()
            .map(|path| display_path(&cwd, PathBuf::from(path).as_path()))
            .collect();
        let mut text = rendered.join("\n");
        if rendered.len() as u32 >= max_results {
            text.push_str(&format!(
                "\n... [limited to {max_results} results; refine the pattern]"
            ));
        }
        Ok(ToolOutput::ok(text))
    }
}

fn display_path(cwd: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
