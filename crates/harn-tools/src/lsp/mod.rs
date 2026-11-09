//! LSP 集成：把外部语言服务器接进 Agent 循环（对齐 Claude Code `src/services/lsp`）。
//!
//! 子模块分工：
//! - [`client`]：stdio JSONRPC 客户端（Content-Length 帧、请求/通知/响应、崩溃回调）。
//! - [`instance`]：单个语言服务器（initialize 握手、capabilities、**位置编码协商**、
//!   文档 didOpen/didChange/didSave/didClose 同步）。
//! - [`manager`]：多服务器按扩展名路由、按需启动、共享诊断注册表。
//! - [`diagnostics`]：`publishDiagnostics` 收集与 severity 排序。
//!
//! 服务器配置类型（[`harn_config::LspServerConfig`]）在 harn-config 的 settings
//! 分层里解析（`lspServers` 段）。
//!
//! 相对 Claude Code 的「更新」：
//! 1. `lsp-types` 0.97（LSP 3.17）替代其 vscode-jsonrpc 绑定；
//! 2. 位置编码协商（UTF-8 优先，回退 UTF-16）——CC 未实现，多字节字符旁跳转偏移；
//! 3. 配置经 settings 分层（user/project/local），不限于插件通道。

use std::str::FromStr;

pub mod client;
pub mod diagnostics;
pub mod format;
pub mod instance;
pub mod manager;
pub mod tool;

pub use diagnostics::DiagnosticRegistry;
pub use manager::LspManager;
pub use tool::LspTool;

/// 本 crate 错误（显式枚举，禁止盒装化丢失分支）。
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("LSP 服务器启动失败（{server}）：{message}")]
    SpawnFailed { server: String, message: String },

    #[error("LSP 服务器 {server} 未初始化")]
    #[allow(dead_code)]
    NotInitialized { server: String },

    #[error("找不到处理文件 {path:?} 的 LSP 服务器")]
    NoServerForFile { path: String },

    #[error("LSP 请求 {method} 失败（{server}）：{message}")]
    RequestFailed {
        server: String,
        method: String,
        message: String,
    },

    #[error("JSONRPC 帧错误：{0}")]
    Framing(String),

    #[error("JSON 编解码错误：{0}")]
    Json(String),

    #[error("IO 错误：{0}")]
    Io(String),

    #[error("服务器 {server} 已崩溃，需重启")]
    Crashed { server: String },
}

impl From<LspError> for harn_core::error::HarnError {
    fn from(value: LspError) -> Self {
        harn_core::error::HarnError::Tool(value.to_string())
    }
}

/// 文件路径 → `file://` URI（lsp-types 0.97 的 `Uri` 只接受字符串构造，
/// 这里用 `url` crate 做权威的 percent-encoding 转换）。
pub fn path_to_uri(path: &std::path::Path) -> Result<lsp_types::Uri, LspError> {
    let absolute = std::path::absolute(path)
        .map_err(|error| LspError::Framing(format!("路径规范化失败：{error}")))?;
    let url = url::Url::from_file_path(&absolute)
        .map_err(|()| LspError::Framing(format!("非 file:// 路径：{}", absolute.display())))?;
    lsp_types::Uri::from_str(url.as_str())
        .map_err(|error| LspError::Framing(format!("URI 构造失败：{error}")))
}

/// `file://` URI → 本地路径。
pub fn uri_to_path(uri: &lsp_types::Uri) -> Result<std::path::PathBuf, LspError> {
    let url = url::Url::parse(uri.as_str())
        .map_err(|error| LspError::Framing(format!("URI 解析失败：{error}")))?;
    url.to_file_path()
        .map_err(|()| LspError::Framing(format!("非 file URI：{}", uri.as_str())))
}

/// 按扩展名猜 languageId（服务器会自行校正；未列举的透传小写扩展名）。
pub fn language_id(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "scala" => "scala",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" | "htm" => "html",
        "css" => "css",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        other => other,
    }
    .to_string()
}
