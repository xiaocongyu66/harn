//! 领域错误。

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HarnError {
    #[error("配置错误：{0}")]
    Config(String),
    #[error("Provider 错误：{0}")]
    Provider(String),
    #[error("权限拒绝：{0}")]
    Permission(String),
    #[error("工具错误：{0}")]
    Tool(String),
    #[error("会话错误：{0}")]
    Session(String),
    #[error("HTTP 错误：{0}")]
    Http(String),
    #[error("已中断")]
    Aborted,
    #[error("路径 `{path}` 不在工作区 `{workspace}` 内")]
    OutsideWorkspace { path: PathBuf, workspace: PathBuf },
    #[error("图片错误：{0}")]
    Image(String),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误：{0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, HarnError>;
