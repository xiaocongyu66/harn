//! Edit 工具：精确字符串替换。

use async_trait::async_trait;
use serde_json::{Value, json};

use harn_core::error::{HarnError, Result};

use super::{Tool, ToolContext, ToolOutput, object_schema};

pub struct EditTool;

impl EditTool {
    fn apply(
        content: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> std::result::Result<(String, usize), String> {
        if old.is_empty() {
            return Err(harn_i18n::tr!("tool.edit.old_string_empty"));
        }
        let count = content.matches(old).count();
        if count == 0 {
            return Err(harn_i18n::tr!("tool.edit.old_string_not_found"));
        }
        if count > 1 && !replace_all {
            return Err(harn_i18n::tr!(
                "tool.edit.old_string_ambiguous",
                count = count
            ));
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        Ok((updated, if replace_all { count } else { 1 }))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a file. `old_string` must uniquely match unless \
         replace_all is true. Read the file first; preserve exact indentation."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "file_path": {
                    "type": "string",
                    "description": "Path to edit, relative to the workspace or absolute."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false)."
                }
            }),
            &["file_path", "old_string", "new_string"],
        )
    }

    fn writes(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.edit.file_path_required")))?;
        let old = input
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.edit.old_string_required")))?;
        let new = input
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.edit.new_string_required")))?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let path = ctx.resolve_path(raw);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.edit.read_failed",
                path = path.display(),
                e = e
            ))
        })?;
        let (updated, replaced) = match Self::apply(&content, old, new, replace_all) {
            Ok(result) => result,
            Err(message) => return Ok(ToolOutput::error(message)),
        };
        std::fs::write(&path, &updated).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.edit.write_failed",
                path = path.display(),
                e = e
            ))
        })?;
        Ok(ToolOutput::ok(format!(
            "Edited {}: replaced {} occurrence(s)\n{}",
            path.display(),
            replaced,
            crate::diff::render_diff_marked(
                &crate::diff::structured_patch(&content, &updated, 3),
                false,
            )
        )))
    }
}

/// 渲染 old→new 的标记化 diff（无 ANSI；行首 +/-/空格 + [n] gutter 行号）。
pub fn diff_marked(old: &str, new: &str) -> String {
    let hunks = crate::diff::structured_patch(old, new, 3);
    if hunks.is_empty() {
        return String::new();
    }
    crate::diff::render_diff_marked(&hunks, false)
}

#[cfg(test)]
mod tests {
    use super::EditTool;

    #[test]
    fn rejects_ambiguous_replacement() {
        let content = "a\na\n";
        assert!(EditTool::apply(content, "a", "b", false).is_err());
        let (updated, count) = EditTool::apply(content, "a", "b", true).unwrap();
        assert_eq!(updated, "b\nb\n");
        assert_eq!(count, 2);
    }

    #[test]
    fn replaces_single_occurrence() {
        let (updated, count) = EditTool::apply("let x = 1;\n", "1", "2", false).unwrap();
        assert_eq!(updated, "let x = 2;\n");
        assert_eq!(count, 1);
    }

    #[test]
    fn missing_old_string_is_error() {
        assert!(EditTool::apply("abc", "z", "y", false).is_err());
    }
}
