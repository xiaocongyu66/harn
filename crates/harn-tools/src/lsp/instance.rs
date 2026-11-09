//! 单个语言服务器实例：握手、capabilities、文档同步、请求转发。
//!
//! 相对 Claude Code 的「更新」：initialize 时做 **位置编码协商**——
//! 若服务器声明支持 `utf-8`（`positionEncodings`），后续行列按字符数传；
//! 否则按 UTF-16 code unit 计算（LSP 3.17 默认）。CC 未做协商，多字节注释/中文字符串
//! 旁的定义跳转会偏移。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, GeneralClientCapabilities,
    InitializeParams, InitializeResult, PositionEncodingKind, ServerCapabilities,
    TextDocumentContentChangeEvent, TextDocumentItem, VersionedTextDocumentIdentifier,
};
use serde_json::json;

use super::client::JsonRpcClient;
use super::diagnostics::DiagnosticRegistry;
use super::{LspError, path_to_uri};

/// 服务器当前协商到的位置编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    /// UTF-8：character 按 Unicode scalar 数（rust-analyzer 等新服务器首选）。
    Utf8,
    /// UTF-16：character 按 code unit 数（LSP 3.17 之前的默认）。
    Utf16,
}

impl PositionEncoding {
    /// 一行的「列偏移」计算（以字符列为输入，转成服务器期望的计数单位）。
    pub fn column_of(&self, line_text: &str, char_index: usize) -> u32 {
        match self {
            Self::Utf8 => char_index as u32,
            Self::Utf16 => line_text
                .chars()
                .take(char_index)
                .map(|ch| ch.len_utf16() as u32)
                .sum(),
        }
    }
}

/// 单个 LSP 服务器实例。
pub struct LspServerInstance {
    /// 服务器名（settings 里的键，如 "rust-analyzer"）。
    pub name: String,
    client: Arc<JsonRpcClient>,
    capabilities: ServerCapabilities,
    encoding: PositionEncoding,
    /// 已打开的文档（path → 版本号），didChange 需要递增值。
    open_docs: std::sync::Mutex<HashMap<PathBuf, i32>>,
    root: PathBuf,
}

impl LspServerInstance {
    /// 启动 + initialize 握手 + initialized 通知。
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        name: String,
        command: &str,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        root_marker: &str,
        cwd: &Path,
        client_capabilities: ClientCapabilities,
        diagnostics: Arc<DiagnosticRegistry>,
        on_crash: Option<Box<dyn FnOnce(String) + Send>>,
    ) -> Result<Arc<Self>, LspError> {
        let root = resolve_root(root_marker, cwd);
        let root_uri = path_to_uri(&root).ok();
        let client = Arc::new(
            JsonRpcClient::start(
                name.clone(),
                command,
                args,
                &env.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                &root,
                on_crash,
            )
            .await?,
        );

        // 诊断通知接线
        let diagnostics_clone = Arc::clone(&diagnostics);
        let diagnostics_name = name.clone();
        client.on_notification(
            "textDocument/publishDiagnostics",
            Arc::new(move |params: serde_json::Value| {
                if let Ok(parsed) =
                    serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params)
                {
                    diagnostics_clone.update(&diagnostics_name, &parsed);
                }
            }),
        );

        let general = GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF8]),
            ..Default::default()
        };
        let mut client_capabilities = client_capabilities;
        client_capabilities.general = Some(general);

        // 0.97：`root_uri` 已 deprecated（用 workspace_folders 表达根）。
        let params = InitializeParams {
            process_id: None,
            workspace_folders: root_uri.map(|uri| {
                vec![lsp_types::WorkspaceFolder {
                    uri,
                    name: "harn".to_string(),
                }]
            }),
            capabilities: client_capabilities,
            client_info: None,
            locale: None,
            initialization_options: None,
            trace: None,
            work_done_progress_params: Default::default(),
            ..Default::default()
        };
        let response = client
            .request(
                "initialize",
                serde_json::to_value(&params).map_err(|e| LspError::Json(e.to_string()))?,
            )
            .await?;
        let result: InitializeResult = serde_json::from_value(response)
            .map_err(|error| LspError::Json(format!("initialize 响应解析失败：{error}")))?;

        // 位置编码协商：优先 utf-8，回退 utf-16（双方能力交集）。
        let encoding = negotiate_encoding(&result.capabilities);
        let capabilities = result.capabilities;

        let _ = client.notify("initialized", json!({})).await;

        Ok(Arc::new(Self {
            name,
            client,
            capabilities,
            encoding,
            open_docs: std::sync::Mutex::new(HashMap::new()),
            root,
        }))
    }

    /// 位置编码（文档行列转换用）。
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn client(&self) -> &Arc<JsonRpcClient> {
        &self.client
    }

    /// 文档是否已打开。
    pub fn is_open(&self, path: &Path) -> bool {
        self.open_docs
            .lock()
            .map(|docs| docs.contains_key(path))
            .unwrap_or(false)
    }

    /// didOpen（服务器要求先 open 再 query）。
    pub async fn open_file(
        &self,
        path: &Path,
        content: &str,
        language_id: &str,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let version = {
            let mut docs = self
                .open_docs
                .lock()
                .map_err(|_| LspError::Framing("锁中毒".into()))?;
            let version = docs.entry(path.to_path_buf()).or_insert(0);
            *version += 1;
            *version
        };
        self.client
            .notify(
                "textDocument/didOpen",
                serde_json::to_value(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri,
                        language_id: language_id.to_string(),
                        version,
                        text: content.to_string(),
                    },
                })
                .map_err(|e| LspError::Json(e.to_string()))?,
            )
            .await
    }

    /// didChange（全量同步：简单可靠；后续可改增量）。
    pub async fn change_file(&self, path: &Path, content: &str) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let version = {
            let mut docs = self
                .open_docs
                .lock()
                .map_err(|_| LspError::Framing("锁中毒".into()))?;
            match docs.get_mut(path) {
                Some(version) => {
                    *version += 1;
                    *version
                }
                None => return Ok(()),
            }
        };
        self.client
            .notify(
                "textDocument/didChange",
                serde_json::to_value(DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri, version },
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: content.to_string(),
                    }],
                })
                .map_err(|e| LspError::Json(e.to_string()))?,
            )
            .await
    }

    /// didSave。
    pub async fn save_file(&self, path: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        self.client
            .notify(
                "textDocument/didSave",
                serde_json::to_value(DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                    text: None,
                })
                .map_err(|e| LspError::Json(e.to_string()))?,
            )
            .await
    }

    /// didClose：从 open_docs 移除，并清空该文件在这台服务器上的诊断。
    pub async fn close_file(&self, path: &Path) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        self.open_docs
            .lock()
            .map_err(|_| LspError::Framing("锁中毒".into()))?
            .remove(path);
        self.client
            .notify(
                "textDocument/didClose",
                serde_json::to_value(DidCloseTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier { uri },
                })
                .map_err(|e| LspError::Json(e.to_string()))?,
            )
            .await
    }

    /// 泛型请求转发（method + 原始 params）。
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        self.client.request(method, params).await
    }

    pub async fn stop(&self) {
        self.client.stop().await;
    }
}

/// 选择双方都支持的位置编码：服务器声明 utf-8 就用 utf-8，否则 LSP 3.17 默认 utf-16。
fn negotiate_encoding(capabilities: &ServerCapabilities) -> PositionEncoding {
    if capabilities.position_encoding == Some(PositionEncodingKind::UTF8) {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

/// root_marker 解析：`.` = cwd；否则向上找含该文件/目录的祖先。
pub fn resolve_root(marker: &str, cwd: &Path) -> PathBuf {
    if marker == "." || marker.is_empty() {
        return cwd.to_path_buf();
    }
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(marker).exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return cwd.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_column_utf16_counts_surrogate_pairs() {
        let line = "a😀b中文c";
        // 字符索引 3 处是 '中'：UTF-16 需把 emoji（2 unit）算进去 → 4
        assert_eq!(PositionEncoding::Utf16.column_of(line, 3), 4);
        assert_eq!(PositionEncoding::Utf8.column_of(line, 3), 3);
    }

    #[test]
    fn resolve_root_dot_means_cwd() {
        let cwd = Path::new("/tmp/project");
        assert_eq!(resolve_root(".", cwd), cwd);
        assert_eq!(resolve_root("", cwd), cwd);
    }

    #[test]
    fn resolve_root_walks_up_to_marker() {
        let base = tempfile::tempdir().unwrap();
        let project = base.path().join("project");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("Cargo.toml"), "").unwrap();
        assert_eq!(resolve_root("Cargo.toml", &nested), project);
        // 找不到标记时退回 cwd
        assert_eq!(resolve_root("nonexistent.marker", &nested), nested);
    }
}
