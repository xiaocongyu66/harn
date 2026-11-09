//! Lsp 工具：九个操作（definition/references/hover/documentSymbol/workspaceSymbol/
//! implementation/callHierarchy 三连）。
//!
//! 移植 CC `packages/builtin-tools/src/tools/LSPTool`：同样的 operation 判别与
//! 1-based 行列入参。差异：
//! - 行列经**协商后的位置编码**转换（UTF-8 直接、UTF-16 按 code unit 折算）；
//! - 工具只发请求，**自动完成 didOpen**（CC 需要 Agent 另调 Edit 后同步）。

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, DocumentSymbol, GotoDefinitionResponse,
    Hover, Location, SymbolInformation,
};
use serde_json::{Value, json};

use super::format;
use super::{LspError, LspManager, language_id};
use crate::{Tool, ToolContext, ToolOutput, object_schema};

/// 九个操作的判别字段。
const OPERATIONS: [&str; 9] = [
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

pub struct LspTool;

impl LspTool {
    /// 未配置任何 LSP 服务器时给模型可执行的引导（而不是干巴巴的报错）。
    fn setup_guidance() -> String {
        "LSP 未配置：settings 的 lspServers 段为空。配置示例（~/.harn/settings.json）：\n\
         {\"lspServers\": {\"rust-analyzer\": {\"command\": \"rust-analyzer\", \
         \"filetypes\": [\"rs\"], \"rootMarker\": \".\"}}}\n\
         服务器必须是系统已安装的语言服务器（harn 不代装）。"
            .to_string()
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn description(&self) -> &'static str {
        "Query Language Server Protocol servers for code intelligence. Operations: \
         goToDefinition (where a symbol is defined), findReferences (all usages), \
         hover (docs/type info), documentSymbol (symbols in one file), \
         workspaceSymbol (symbols across the workspace, takes query), \
         goToImplementation (implementations of an interface/abstract method), \
         prepareCallHierarchy (call-hierarchy item at a position), \
         incomingCalls (who calls this), outgoingCalls (what this calls). \
         Position arguments are 1-based, as shown in editors. Returns an error when no \
         LSP server covers the file type."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "operation": {
                    "type": "string",
                    "enum": OPERATIONS,
                    "description": "Which LSP operation to run."
                },
                "filePath": {
                    "type": "string",
                    "description": "File to operate on (absolute or relative to cwd)."
                },
                "line": {
                    "type": "integer",
                    "description": "1-based line number (ignored by workspaceSymbol)."
                },
                "character": {
                    "type": "integer",
                    "description": "1-based character offset (ignored by workspaceSymbol)."
                },
                "query": {
                    "type": "string",
                    "description": "Symbol query for workspaceSymbol."
                }
            }),
            &["operation", "filePath"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // 全部九个操作都是查询；didOpen 同步是本地副作用，不算工作区写入。
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(manager) = &ctx.lsp else {
            return Ok(ToolOutput::error(Self::setup_guidance()));
        };
        let operation = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool("lsp 需要 `operation`".into()))?;
        if !OPERATIONS.contains(&operation) {
            return Err(HarnError::Tool(format!(
                "未知 operation：{operation:?}（可用：{}）",
                OPERATIONS.join(", ")
            )));
        }
        let raw_path = input
            .get("filePath")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| HarnError::Tool("lsp 需要 `filePath`".into()))?;
        let path = ctx.resolve_path(raw_path);

        // workspaceSymbol 不需要位置
        if operation == "workspaceSymbol" {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            let response: serde_json::Value = manager
                .send_request(&path, "workspace/symbol", json!({ "query": query }))
                .await?;
            let symbols: Vec<SymbolInformation> =
                serde_json::from_value(response).unwrap_or_default();
            let symbols: Vec<SymbolInformation> = symbols
                .into_iter()
                .filter(|symbol| !symbol.location.uri.as_str().is_empty())
                .collect();
            return Ok(ToolOutput::ok(format::format_workspace_symbols(
                &symbols, &ctx.cwd,
            )));
        }

        // 位置参数（1-based → 0-based，再按协商编码折算列）
        let line = input
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| HarnError::Tool("lsp 需要 `line`（1-based）".into()))?;
        let character = input
            .get("character")
            .and_then(Value::as_u64)
            .ok_or_else(|| HarnError::Tool("lsp 需要 `character`（1-based）".into()))?;
        if line == 0 || character == 0 {
            return Err(HarnError::Tool("lsp 的 line/character 从 1 开始".into()));
        }
        let line = line - 1;
        let char_index = (character - 1) as usize;

        let position = lsp_position(manager, &path, line, char_index).await?;
        let uri = super::path_to_uri(&path)?.as_str().to_string();
        let text_document = json!({ "uri": uri });
        let position = json!({ "line": position.line, "character": position.character });

        match operation {
            "goToDefinition" | "goToImplementation" => {
                let method = if operation == "goToDefinition" {
                    "textDocument/definition"
                } else {
                    "textDocument/implementation"
                };
                let response = manager
                    .send_request(
                        &path,
                        method,
                        json!({"textDocument": text_document, "position": position}),
                    )
                    .await?;
                let result: GotoDefinitionResponse = serde_json::from_value(response)
                    .unwrap_or(GotoDefinitionResponse::Array(Vec::new()));
                Ok(ToolOutput::ok(format::format_definition(&result, &ctx.cwd)))
            }
            "findReferences" => {
                let response = manager
                    .send_request(
                        &path,
                        "textDocument/references",
                        json!({
                            "textDocument": text_document,
                            "position": position,
                            "context": {"includeDeclaration": true}
                        }),
                    )
                    .await?;
                let locations: Vec<Location> = serde_json::from_value(response).unwrap_or_default();
                let filtered: Vec<Location> = locations
                    .into_iter()
                    .filter(|location| !location.uri.as_str().is_empty())
                    .collect();
                Ok(ToolOutput::ok(format::format_references(
                    &filtered, &ctx.cwd,
                )))
            }
            "hover" => {
                let response = manager
                    .send_request(
                        &path,
                        "textDocument/hover",
                        json!({"textDocument": text_document, "position": position}),
                    )
                    .await?;
                let hover: Option<Hover> = serde_json::from_value(response).ok();
                match hover {
                    Some(hover) => Ok(ToolOutput::ok(format::format_hover(&hover, &ctx.cwd))),
                    None => Ok(ToolOutput::ok(
                        "No hover information available.".to_string(),
                    )),
                }
            }
            "documentSymbol" => {
                ensure_document_open(manager, &path).await?;
                let response = manager
                    .send_request(
                        &path,
                        "textDocument/documentSymbol",
                        json!({"textDocument": text_document}),
                    )
                    .await?;
                // LSP 允许两种返回形态：DocumentSymbol[]（树）或 SymbolInformation[]（扁平）。
                // 先按树解析，失败再按扁平解——真实服务器（如 pylsp）常用后者。
                let text = match serde_json::from_value::<Vec<DocumentSymbol>>(response.clone()) {
                    Ok(symbols) => format::format_document_symbols(&symbols),
                    Err(_) => {
                        let flat: Vec<SymbolInformation> =
                            serde_json::from_value(response).unwrap_or_default();
                        format::format_symbol_informations(&flat)
                    }
                };
                Ok(ToolOutput::ok(text))
            }
            "prepareCallHierarchy" => {
                let response = manager
                    .send_request(
                        &path,
                        "textDocument/prepareCallHierarchy",
                        json!({"textDocument": text_document, "position": position}),
                    )
                    .await?;
                let items: Vec<lsp_types::CallHierarchyItem> =
                    serde_json::from_value(response).unwrap_or_default();
                Ok(ToolOutput::ok(format::format_prepare_call_hierarchy(
                    &items, &ctx.cwd,
                )))
            }
            "incomingCalls" | "outgoingCalls" => {
                // 两步：先 prepare 拿 item，再用 item 发 callHierarchy 请求
                let prepare_response = manager
                    .send_request(
                        &path,
                        "textDocument/prepareCallHierarchy",
                        json!({"textDocument": text_document, "position": position}),
                    )
                    .await?;
                let items: Vec<lsp_types::CallHierarchyItem> =
                    serde_json::from_value(prepare_response).unwrap_or_default();
                let Some(item) = items.into_iter().next() else {
                    return Ok(ToolOutput::ok(
                        "No call hierarchy item found at this position".to_string(),
                    ));
                };
                let item_value = serde_json::to_value(&item)
                    .map_err(|error| HarnError::Tool(format!("item 序列化失败：{error}")))?;
                let method = if operation == "incomingCalls" {
                    "callHierarchy/incomingCalls"
                } else {
                    "callHierarchy/outgoingCalls"
                };
                let response = manager
                    .send_request(&path, method, json!({ "item": item_value }))
                    .await?;
                if operation == "incomingCalls" {
                    let calls: Vec<CallHierarchyIncomingCall> =
                        serde_json::from_value(response).unwrap_or_default();
                    Ok(ToolOutput::ok(format::format_incoming_calls(
                        &calls, &ctx.cwd,
                    )))
                } else {
                    let calls: Vec<CallHierarchyOutgoingCall> =
                        serde_json::from_value(response).unwrap_or_default();
                    Ok(ToolOutput::ok(format::format_outgoing_calls(
                        &calls, &ctx.cwd,
                    )))
                }
            }
            other => Err(HarnError::Tool(format!("operation {other:?} 未实现"))),
        }
    }
}

/// 确保服务器已打开该文档（didOpen 是 LSP 查询的前置要求，与 CC 的显式
/// openFile 不同：harn 的 Lsp 工具自己补上这一步，模型无需先读文件）。
pub async fn ensure_document_open(manager: &LspManager, path: &std::path::Path) -> Result<()> {
    let server = manager
        .ensure_server(path)
        .await?
        .ok_or_else(|| LspError::NoServerForFile {
            path: path.display().to_string(),
        })?;
    if !server.is_open(path) {
        let content = std::fs::read_to_string(path)
            .map_err(|error| LspError::Io(format!("读取 {} 失败：{error}", path.display())))?;
        let language = language_id(path);
        server.open_file(path, &content, &language).await?;
    }
    Ok(())
}

/// 取该文件生效的位置编码，把（行, 字符列）转成服务器期望的 LSP Position，
/// 并确保服务器已打开该文档（didOpen 对查询是前置要求）。
async fn lsp_position(
    manager: &LspManager,
    path: &std::path::Path,
    line: u64,
    char_index: usize,
) -> Result<lsp_types::Position> {
    // 路由/启动检查：没有服务器覆盖该文件类型时给出明确错误
    let server = manager
        .ensure_server(path)
        .await?
        .ok_or_else(|| LspError::NoServerForFile {
            path: path.display().to_string(),
        })?;
    if !server.is_open(path) {
        let content = std::fs::read_to_string(path)
            .map_err(|error| LspError::Io(format!("读取 {} 失败：{error}", path.display())))?;
        let language = language_id(path);
        server.open_file(path, &content, &language).await?;
    }
    let encoding = server.encoding();
    let line_text = std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().nth(line as usize).map(str::to_string))
        .unwrap_or_default();
    let column = encoding.column_of(&line_text, char_index);
    Ok(lsp_types::Position {
        line: line as u32,
        character: column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;

    #[tokio::test]
    async fn lsp_tool_without_manager_returns_setup_guidance() {
        let tool = LspTool;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(dir.path().to_path_buf());
        let output = tool
            .call(
                json!({"operation": "hover", "filePath": "main.rs", "line": 1, "character": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("lspServers"), "{}", output.content);
    }

    #[tokio::test]
    async fn lsp_tool_rejects_zero_position() {
        let tool = LspTool;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::for_test(dir.path().to_path_buf());
        // 空配置管理器（无服务器）——先过位置校验再走"无服务器"路径
        let manager = LspManager::new(
            harn_config::Settings::default().lsp_servers,
            dir.path().to_path_buf(),
            std::sync::Arc::new(super::super::DiagnosticRegistry::new()),
            lsp_types::ClientCapabilities::default(),
        );
        ctx.lsp = Some(manager);
        let output = tool
            .call(
                json!({"operation": "hover", "filePath": "main.rs", "line": 0, "character": 1}),
                &ctx,
            )
            .await;
        assert!(output.is_err(), "line=0 应被拒绝");
    }

    /// 回归：pylsp 等服务器的 documentSymbol 返回 **SymbolInformation[]**（扁平形态）。
    /// 曾因只解析 DocumentSymbol[] 而静默输出空——真实流量抓到后修复。
    #[test]
    fn document_symbol_accepts_symbol_information_shape() {
        // 摘自 pylsp 的真实响应（trace 抓取）
        let response = json!([
            {"name": "greet", "containerName": null,
             "location": {"uri": "file:///tmp/x/app.py",
                          "range": {"start": {"line": 3, "character": 0},
                                    "end": {"line": 6, "character": 0}}},
             "kind": 12},
            {"name": "main", "containerName": null,
             "location": {"uri": "file:///tmp/x/app.py",
                          "range": {"start": {"line": 8, "character": 0},
                                    "end": {"line": 9, "character": 20}}},
             "kind": 12}
        ]);
        // 树形解析必然失败（无 selectionRange），回落扁平形态必须成功
        assert!(serde_json::from_value::<Vec<DocumentSymbol>>(response.clone()).is_err());
        let flat: Vec<SymbolInformation> = serde_json::from_value(response).unwrap_or_default();
        assert_eq!(flat.len(), 2, "SymbolInformation[] 必须解析成功");
        assert_eq!(flat[0].name, "greet");
        assert_eq!(flat[0].kind, lsp_types::SymbolKind::FUNCTION);
        assert_eq!(flat[0].location.range.start.line, 3);
        // 格式化输出包含 1-based 行号
        let text = format::format_symbol_informations(&flat);
        assert!(text.contains("greet"), "{text}");
        assert!(text.contains("L4:1"), "{text}");
    }

    /// 回归：树形形态（rust-analyzer 风格）仍走原路径。
    #[test]
    fn document_symbol_accepts_document_symbol_shape() {
        let response = json!([{
            "name": "outer", "kind": 12,
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 9, "character": 1}},
            "selectionRange": {"start": {"line": 5, "character": 3}, "end": {"line": 5, "character": 8}},
            "children": [{
                "name": "inner", "kind": 13,
                "range": {"start": {"line": 6, "character": 4}, "end": {"line": 6, "character": 9}},
                "selectionRange": {"start": {"line": 6, "character": 4}, "end": {"line": 6, "character": 9}}
            }]
        }]);
        let tree: Vec<DocumentSymbol> = serde_json::from_value(response).unwrap();
        let text = format::format_document_symbols(&tree);
        assert!(text.contains("outer"), "{text}");
        assert!(text.contains("  inner"), "子符号应缩进：{text}");
    }
}
