//! NotebookEdit 工具：编辑 `.ipynb`（nbformat 4.x）的 cell（移植 Claude Code
//! NotebookEdit 的语义：按 id 或索引 replace/insert/delete，保留其余结构）。

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellEditMode {
    Replace,
    Insert,
    Delete,
}

impl CellEditMode {
    fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "insert" => Self::Insert,
            "delete" => Self::Delete,
            _ => Self::Replace,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Insert => "insert",
            Self::Delete => "delete",
        }
    }
}

/// cell.source 可能是字符串或行数组（nbformat 两种形态）——统一为字符串。
#[allow(dead_code)]
pub fn cell_source_to_string(source: &Value) -> String {
    match source {
        Value::String(text) => text.clone(),
        Value::Array(lines) => lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

fn string_to_source_lines(text: &str) -> Vec<Value> {
    // nbformat 惯例：每行保留 `\n`（末行除外，除非以换行结束）。
    let mut lines: Vec<Value> = text
        .split_inclusive('\n')
        .map(|line| Value::String(line.to_string()))
        .collect();
    if lines.is_empty() {
        lines.push(Value::String(String::new()));
    }
    lines
}

fn cell_id(cell: &Value, index: usize) -> String {
    cell.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{index}"))
}

/// 在 notebook JSON 上应用 edit；返回（新 notebook，人类可读摘要）。
pub fn apply_notebook_edit(
    notebook: &mut Value,
    cell_selector: &str,
    cell_type: &str,
    new_source: Option<&str>,
    mode: CellEditMode,
) -> Result<String> {
    let cells = notebook
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.notebook.invalid_notebook")))?;
    if !matches!(cell_type, "code" | "markdown" | "raw") && mode != CellEditMode::Delete {
        return Err(HarnError::Tool(harn_i18n::tr!(
            "tool.notebook.bad_cell_type",
            cell_type = cell_type
        )));
    }

    let index = if let Ok(position) = cell_selector.trim_start_matches('#').parse::<usize>() {
        if position >= cells.len() {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "tool.notebook.index_out_of_range",
                position = position,
                total = cells.len()
            )));
        }
        position
    } else {
        cells
            .iter()
            .position(|cell| cell_id(cell, 0) == cell_selector)
            .ok_or_else(|| {
                HarnError::Tool(harn_i18n::tr!(
                    "tool.notebook.cell_not_found",
                    id = cell_selector
                ))
            })?
    };

    match mode {
        CellEditMode::Replace => {
            let source = new_source.ok_or_else(|| {
                HarnError::Tool(harn_i18n::tr!("tool.notebook.replace_requires_source"))
            })?;
            let cell = cells
                .get_mut(index)
                .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.notebook.invalid_index")))?;
            if cell.get("cell_type").and_then(Value::as_str) != Some(cell_type) {
                cell.as_object_mut()
                    .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.notebook.not_object")))?
                    .insert(
                        "cell_type".to_string(),
                        Value::String(cell_type.to_string()),
                    );
            }
            cell.as_object_mut()
                .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.notebook.not_object")))?
                .insert(
                    "source".to_string(),
                    Value::Array(string_to_source_lines(source)),
                );
            Ok(format!(
                "replaced cell {} ({cell_type})",
                cell_id(&cells[index], index)
            ))
        }
        CellEditMode::Insert => {
            let source = new_source.ok_or_else(|| {
                HarnError::Tool(harn_i18n::tr!("tool.notebook.insert_requires_source"))
            })?;
            let id = format!("c{}", uuid::Uuid::new_v4().simple());
            let mut cell = serde_json::Map::new();
            cell.insert("id".to_string(), Value::String(id.clone()));
            cell.insert(
                "cell_type".to_string(),
                Value::String(cell_type.to_string()),
            );
            cell.insert(
                "source".to_string(),
                Value::Array(string_to_source_lines(source)),
            );
            if cell_type == "code" {
                cell.insert("outputs".to_string(), Value::Array(Vec::new()));
                cell.insert("execution_count".to_string(), Value::Null);
            }
            cell.insert("metadata".to_string(), json!({}));
            cells.insert(index + 1, Value::Object(cell));
            Ok(format!("inserted {cell_type} cell {id} at #{index}"))
        }
        CellEditMode::Delete => {
            let removed = cells.remove(index);
            Ok(format!("deleted cell {}", cell_id(&removed, index)))
        }
    }
}

pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &'static str {
        "NotebookEdit"
    }

    fn description(&self) -> &'static str {
        "Edit a Jupyter notebook (.ipynb) cell: replace its source, insert a new cell after \
         the selected one, or delete it. Select the cell by id or by #index."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "notebook_path": {"type": "string", "description": "Absolute path to the .ipynb file."},
                "cell_id": {"type": "string", "description": "Cell id, or #index (0-based)."},
                "cell_type": {"type": "string", "enum": ["code", "markdown", "raw"]},
                "new_source": {"type": "string", "description": "New cell source (required for replace/insert)."},
                "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
            }),
            &["notebook_path", "cell_id", "cell_type", "edit_mode"],
        )
    }

    fn writes(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw = input
            .get("notebook_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                HarnError::Tool(harn_i18n::tr!("tool.notebook.notebook_path_required"))
            })?;
        let cell_selector = input
            .get("cell_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.notebook.cell_id_required")))?
            .to_string();
        let cell_type = input
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code")
            .to_string();
        let new_source = input
            .get("new_source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mode = CellEditMode::from_str(
            input
                .get("edit_mode")
                .and_then(Value::as_str)
                .unwrap_or("replace"),
        );

        let path = ctx.resolve_path(raw);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.notebook.read_failed",
                path = path.display(),
                e = e
            ))
        })?;
        let mut notebook: Value = serde_json::from_str(&text)
            .map_err(|e| HarnError::Tool(harn_i18n::tr!("tool.notebook.parse_failed", e = e)))?;

        let summary = apply_notebook_edit(
            &mut notebook,
            &cell_selector,
            &cell_type,
            new_source.as_deref(),
            mode,
        )?;
        let serialized = serde_json::to_string_pretty(&notebook).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!("tool.notebook.serialize_failed", e = e))
        })?;
        std::fs::write(&path, format!("{serialized}\n")).map_err(|e| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.notebook.write_failed",
                path = path.display(),
                e = e
            ))
        })?;
        Ok(ToolOutput::ok(format!(
            "{} [{}] {}",
            summary,
            mode.as_str(),
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "cells": [
                {"cell_type": "markdown", "id": "m1", "metadata": {}, "source": ["# Title\n"]},
                {"cell_type": "code", "id": "c1", "metadata": {}, "execution_count": 1,
                 "outputs": [], "source": ["print('old')\n"]}
            ],
            "metadata": {"kernelspec": {"name": "python3"}},
            "nbformat": 4,
            "nbformat_minor": 5
        })
    }

    #[test]
    fn replaces_cell_source_by_id() {
        let mut notebook = fixture();
        let summary = apply_notebook_edit(
            &mut notebook,
            "c1",
            "code",
            Some("print('new')"),
            CellEditMode::Replace,
        )
        .unwrap();
        assert!(summary.contains("c1"), "{summary}");
        let source = &notebook["cells"][1]["source"];
        assert_eq!(cell_source_to_string(source), "print('new')");
        // 其他字段保留
        assert_eq!(notebook["nbformat"], 4);
        assert_eq!(notebook["cells"][0]["id"], "m1");
    }

    #[test]
    fn inserts_after_index() {
        let mut notebook = fixture();
        apply_notebook_edit(
            &mut notebook,
            "#1",
            "markdown",
            Some("## Section"),
            CellEditMode::Insert,
        )
        .unwrap();
        let cells = notebook["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cell_source_to_string(&cells[2]["source"]), "## Section");
        assert_eq!(cells[2]["cell_type"], "markdown");
    }

    #[test]
    fn deletes_cell() {
        let mut notebook = fixture();
        apply_notebook_edit(&mut notebook, "#0", "markdown", None, CellEditMode::Delete).unwrap();
        let cells = notebook["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["id"], "c1");
    }

    #[test]
    fn rejects_unknown_cell_and_bad_type() {
        let mut notebook = fixture();
        assert!(
            apply_notebook_edit(
                &mut notebook,
                "zzz",
                "code",
                Some("x"),
                CellEditMode::Replace
            )
            .is_err()
        );
        assert!(
            apply_notebook_edit(
                &mut notebook,
                "#5",
                "code",
                Some("x"),
                CellEditMode::Replace
            )
            .is_err()
        );
        assert!(
            apply_notebook_edit(
                &mut notebook,
                "c1",
                "ruby",
                Some("x"),
                CellEditMode::Replace
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn edits_real_notebook_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.ipynb");
        std::fs::write(&path, fixture().to_string()).unwrap();
        let tool = NotebookEditTool;
        let out = tool
            .call(
                json!({
                    "notebook_path": path.to_str().unwrap(),
                    "cell_id": "#1",
                    "cell_type": "code",
                    "new_source": "print('done')",
                    "edit_mode": "replace"
                }),
                &crate::ToolContext::for_test(dir.path().to_path_buf()),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let reloaded: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cell_source_to_string(&reloaded["cells"][1]["source"]),
            "print('done')"
        );
    }
}
