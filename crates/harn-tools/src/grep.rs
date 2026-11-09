//! Grep 工具：基于 harn-search 的进程内 ripgrep 管线。

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use harn_search::{GrepOptions, GrepOutputMode, grep_search_sync};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

const MAX_OUTPUT_CHARS: usize = 30_000;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression using an in-process ripgrep engine. \
         Supports content/files_with_matches/count modes, context lines, glob and type filters."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search; defaults to the workspace root."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob filter, e.g. `*.rs` or `!target/**`."
                },
                "type": {
                    "type": "string",
                    "description": "File type filter (rg --type), e.g. rust, ts, py."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default content)."
                },
                "-i": {"type": "boolean", "description": "Case-insensitive search."},
                "-n": {"type": "boolean", "description": "Include line numbers (always on)."},
                "-A": {"type": "integer", "description": "Lines of context after a match."},
                "-B": {"type": "integer", "description": "Lines of context before a match."},
                "-C": {"type": "integer", "description": "Lines of context before and after."},
                "-o": {"type": "boolean", "description": "Only print the matching part of each line."},
                "-L": {"type": "boolean", "description": "Files without matches."},
                "multiline": {"type": "boolean", "description": "Enable multiline matching."},
                "head_limit": {"type": "integer", "description": "Limit total matches."},
                "offset": {"type": "integer", "description": "Skip the first N matches."},
                "hidden": {"type": "boolean", "description": "Include hidden files (default true)."},
                "gitignore": {"type": "boolean", "description": "Respect .gitignore (default true)."},
                "max_columns": {"type": "integer", "description": "Truncate long lines at N bytes."}
            }),
            &["pattern"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.grep.pattern_required")))?;

        let mode = match input
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("content")
        {
            "files_with_matches" => GrepOutputMode::FilesWithMatches,
            "count" => GrepOutputMode::Count,
            _ => GrepOutputMode::Content,
        };
        let context_c = input.get("-C").and_then(Value::as_u64);
        let before = input
            .get("-B")
            .and_then(Value::as_u64)
            .or(context_c)
            .unwrap_or(0) as u32;
        let after = input
            .get("-A")
            .and_then(Value::as_u64)
            .or(context_c)
            .unwrap_or(0) as u32;
        let only_matching = input.get("-o").and_then(Value::as_bool).unwrap_or(false);

        let options = GrepOptions {
            pattern: pattern.to_string(),
            path: input
                .get("path")
                .and_then(Value::as_str)
                .map(|raw| ctx.resolve_path(raw).to_string_lossy().into_owned()),
            glob: input
                .get("glob")
                .and_then(Value::as_str)
                .map(str::to_string),
            r#type: input
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string),
            ignore_case: input.get("-i").and_then(Value::as_bool),
            multiline: input.get("multiline").and_then(Value::as_bool),
            hidden: Some(input.get("hidden").and_then(Value::as_bool).unwrap_or(true)),
            gitignore: Some(
                input
                    .get("gitignore")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
            max_count: input
                .get("head_limit")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            offset: input
                .get("offset")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            context_before: Some(before),
            context_after: Some(after),
            max_columns: input
                .get("max_columns")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            only_matching: Some(only_matching),
            files_without_match: input.get("-L").and_then(Value::as_bool),
            binary_skip: Some(true),
            mode: Some(mode),
            timeout_ms: Some(20_000),
            cancel_key: Some(ctx.agent_id.clone()),
        };

        let result = tokio::task::spawn_blocking(move || grep_search_sync(options))
            .await
            .map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.grep.task_failed", e = e)))?;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return Ok(ToolOutput::error(harn_i18n::tr!(
                    "tool.grep.failed",
                    error = error
                )));
            }
        };
        if result.cancelled {
            return Ok(ToolOutput::error(harn_i18n::tr!("tool.grep.cancelled")));
        }
        if result.matches.is_empty() {
            return Ok(ToolOutput::ok("(no matches)"));
        }

        let cwd = ctx.cwd.clone();
        let mut output = String::new();
        match mode {
            GrepOutputMode::FilesWithMatches => {
                let mut seen = std::collections::BTreeSet::new();
                for hit in &result.matches {
                    seen.insert(display_path(&cwd, &hit.path));
                }
                output.push_str(&seen.into_iter().collect::<Vec<_>>().join("\n"));
            }
            GrepOutputMode::Count => {
                for hit in &result.matches {
                    output.push_str(&format!("{}:{}\n", display_path(&cwd, &hit.path), hit.line));
                }
            }
            GrepOutputMode::Content => {
                for hit in &result.matches {
                    let path = display_path(&cwd, &hit.path);
                    for (index, line) in hit.before.iter().enumerate() {
                        let line_no =
                            hit.line_number as i64 - hit.before.len() as i64 + index as i64;
                        output.push_str(&format!("{path}-{line_no}- {line}\n"));
                    }
                    output.push_str(&format!(
                        "{path}:{}: {}{}\n",
                        hit.line_number,
                        hit.line,
                        if hit.truncated.unwrap_or(false) {
                            " [truncated]"
                        } else {
                            ""
                        }
                    ));
                    for (index, line) in hit.after.iter().enumerate() {
                        let line_no = hit.line_number as i64 + 1 + index as i64;
                        output.push_str(&format!("{path}-{line_no}- {line}\n"));
                    }
                }
            }
        }
        if result.limit_reached {
            output.push_str("\n... [limit reached; refine the pattern or raise head_limit]");
        }
        output.push_str(&format!(
            "\n({} matches in {} files, {} files searched)",
            result.total_matches, result.files_with_matches, result.files_searched
        ));
        if output.chars().count() > MAX_OUTPUT_CHARS {
            output = output.chars().take(MAX_OUTPUT_CHARS).collect();
            output.push_str("\n... [output truncated]");
        }
        Ok(ToolOutput::ok(output))
    }
}

fn display_path(cwd: &std::path::Path, raw: &str) -> String {
    let path = std::path::Path::new(raw);
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
