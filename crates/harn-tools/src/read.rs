//! Read 工具：带行号的文本读取。

use async_trait::async_trait;
use serde_json::{Value, json};

use harn_core::error::{HarnError, Result};

use super::{Tool, ToolContext, ToolOutput, object_schema};

const DEFAULT_LIMIT: usize = 2_000;
const MAX_LINES: usize = 20_000;
const MAX_CHARS: usize = 120_000;

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn description(&self) -> &'static str {
        "Read a text file from the workspace. Returns line-numbered content. \
         Use offset/limit for large files. Binary files are rejected."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, relative to the workspace or absolute."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start reading from."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default 2000)."
                }
            }),
            &["file_path"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.read.file_path_required")))?;
        let path = ctx.resolve_path(raw);
        let metadata = std::fs::metadata(&path).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.read.metadata_failed",
                path = path.display(),
                e = e
            ))
        })?;
        if metadata.is_dir() {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.read.is_directory",
                path = path.display()
            )));
        }

        let bytes = std::fs::read(&path).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.read.read_failed",
                path = path.display(),
                e = e
            ))
        })?;
        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.read.binary_file",
                path = path.display()
            )));
        }
        let text = String::from_utf8_lossy(&bytes);

        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LINES);

        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Ok(ToolOutput::ok("(empty file)"));
        }
        if offset > lines.len() {
            return Ok(ToolOutput::ok(format!(
                "(offset {offset} beyond end of file; {} lines total)",
                lines.len()
            )));
        }
        let start = offset - 1;
        let end = (start + limit).min(lines.len());
        let width = (end + 1).to_string().len().max(6);
        let mut out = String::new();
        let mut chars = 0usize;
        for (index, line) in lines[start..end].iter().enumerate() {
            let line_no = start + index + 1;
            let rendered = format!("{line_no:>width$}\t{line}\n");
            chars += rendered.chars().count();
            if chars > MAX_CHARS {
                out.push_str("... [output truncated; use offset/limit to read more]\n");
                break;
            }
            out.push_str(&rendered);
        }
        if end < lines.len() && !out.ends_with("read more]\n") {
            out.push_str(&format!(
                "... [{} more lines]\n",
                lines.len().saturating_sub(end)
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}
