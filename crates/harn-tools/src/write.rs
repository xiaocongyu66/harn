//! Write 工具：创建或覆盖文件。

use async_trait::async_trait;
use serde_json::{Value, json};

use harn_core::error::{HarnError, Result};

use super::{Tool, ToolContext, ToolOutput, object_schema};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a file with the given content. Parent directories are created. \
         Prefer Edit for small changes to existing files."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "file_path": {
                    "type": "string",
                    "description": "Path to write, relative to the workspace or absolute."
                },
                "content": {
                    "type": "string",
                    "description": "Full file content."
                }
            }),
            &["file_path", "content"],
        )
    }

    fn writes(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.write.file_path_required")))?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.write.content_required")))?;
        let path = ctx.resolve_path(raw);
        if path.is_dir() {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.write.is_directory",
                path = path.display()
            )));
        }
        let previous = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.write.mkdir_failed", e = e)))?;
        }
        std::fs::write(&path, content).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.write.write_failed",
                path = path.display(),
                e = e
            ))
        })?;
        let diff = if previous == content {
            String::new()
        } else {
            format!("\n{}", super::edit::diff_marked(&previous, content))
        };
        Ok(ToolOutput::ok(format!(
            "Wrote {} bytes to {}{diff}",
            content.len(),
            path.display()
        )))
    }
}
