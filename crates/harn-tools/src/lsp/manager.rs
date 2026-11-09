//! 多服务器管理器：按文件扩展名路由、按需启动、共享诊断注册表。
//!
//! 对齐 CC `LSPServerManager` 的 API 面（ensureServerStarted / sendRequest /
//! openFile / changeFile / saveFile / isFileOpen），用 Arc 共享注册表而非闭包状态机。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{ClientCapabilities, Uri};
use tokio::sync::RwLock;

use harn_config::LspServersConfig;

use super::LspError;
use super::diagnostics::DiagnosticRegistry;
use super::instance::{LspServerInstance, PositionEncoding};

/// LSP 管理器（进程级共享）。
pub struct LspManager {
    servers: RwLock<HashMap<String, Arc<LspServerInstance>>>,
    config: LspServersConfig,
    cwd: PathBuf,
    diagnostics: Arc<DiagnosticRegistry>,
    capabilities: ClientCapabilities,
}

impl LspManager {
    /// 构造管理器（`capabilities` 声明客户端能力：UTF-8 位置编码等）。
    pub fn new(
        config: LspServersConfig,
        cwd: PathBuf,
        diagnostics: Arc<DiagnosticRegistry>,
        capabilities: ClientCapabilities,
    ) -> Arc<Self> {
        Arc::new(Self {
            servers: RwLock::new(HashMap::new()),
            config,
            cwd,
            diagnostics,
            capabilities,
        })
    }

    /// 已启动的全部服务器。
    pub async fn servers(&self) -> Vec<Arc<LspServerInstance>> {
        self.servers.read().await.values().cloned().collect()
    }

    /// 确保该文件的服务器已启动，返回实例。
    pub async fn ensure_server(
        &self,
        path: &Path,
    ) -> Result<Option<Arc<LspServerInstance>>, LspError> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let candidates = self.config.servers_for_extension(&extension);
        if candidates.is_empty() {
            return Ok(None);
        }
        let (name, config) = candidates[0];
        {
            let mut servers = self.servers.write().await;
            if let Some(existing) = servers.get(name) {
                if !existing.client().is_closed() {
                    return Ok(Some(Arc::clone(existing)));
                }
                // 崩溃过的实例移除后重启
                servers.remove(name);
            }
        }
        let instance = LspServerInstance::start(
            name.to_string(),
            &config.command,
            &config.args,
            &config.env,
            &config.root_marker,
            &self.cwd,
            self.capabilities.clone(),
            Arc::clone(&self.diagnostics),
            None,
        )
        .await?;
        self.servers
            .write()
            .await
            .insert(name.to_string(), Arc::clone(&instance));
        Ok(Some(instance))
    }

    /// 已启动的服务器里找负责该文件的（不启动）。
    pub async fn server_for_file(&self, path: &Path) -> Option<Arc<LspServerInstance>> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let servers = self.servers.read().await;
        for (name, config) in &self.config.0 {
            if config
                .filetypes
                .iter()
                .any(|filetype| filetype == &extension)
                && let Some(server) = servers.get(name)
                && !server.client().is_closed()
            {
                return Some(Arc::clone(server));
            }
        }
        None
    }

    /// 转发请求（自动 ensure server）。
    pub async fn send_request(
        &self,
        path: &Path,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let server = self
            .ensure_server(path)
            .await?
            .ok_or_else(|| LspError::NoServerForFile {
                path: path.display().to_string(),
            })?;
        server.request(method, params).await
    }

    /// didOpen 同步（幂等：已开则跳过）。
    pub async fn open_file(&self, path: &Path, content: &str) -> Result<(), LspError> {
        let Some(server) = self.ensure_server(path).await? else {
            return Ok(());
        };
        if server.is_open(path) {
            return Ok(());
        }
        let language = super::language_id(path);
        server.open_file(path, content, &language).await
    }

    /// didChange 同步（未打开则先 didOpen）。
    pub async fn change_file(&self, path: &Path, content: &str) -> Result<(), LspError> {
        let Some(server) = self.ensure_server(path).await? else {
            return Ok(());
        };
        if !server.is_open(path) {
            let language = super::language_id(path);
            server.open_file(path, content, &language).await?;
            return Ok(());
        }
        server.change_file(path, content).await
    }

    pub async fn save_file(&self, path: &Path) -> Result<(), LspError> {
        if let Some(server) = self.server_for_file(path).await {
            server.save_file(path).await?;
        }
        Ok(())
    }

    pub async fn is_file_open(&self, path: &Path) -> bool {
        if let Some(server) = self.server_for_file(path).await {
            return server.is_open(path);
        }
        false
    }

    /// 优雅关闭全部服务器（会话收尾；进程 kill 兜底清理服务器侧文档状态）。
    pub async fn shutdown(&self) {
        let mut servers = self.servers.write().await;
        for (_, server) in servers.drain() {
            server.stop().await;
        }
        drop(servers);
        self.diagnostics.clear();
    }

    /// 该文件生效的位置编码（未启动按 UTF-16 保守处理）。
    pub async fn encoding_for(&self, path: &Path) -> PositionEncoding {
        if let Some(server) = self.server_for_file(path).await {
            return server.encoding();
        }
        PositionEncoding::Utf16
    }

    /// 诊断快捷入口。
    pub fn diagnostics(&self) -> &Arc<DiagnosticRegistry> {
        &self.diagnostics
    }

    /// 文件 URI（工具侧构造请求参数用）。
    pub fn uri_for(path: &Path) -> Result<Uri, LspError> {
        super::path_to_uri(path)
    }
}
