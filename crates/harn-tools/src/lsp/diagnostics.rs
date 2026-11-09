//! 诊断注册表：收集 `textDocument/publishDiagnostics` 通知。
//!
//! 移植对齐 CC `LSPDiagnosticRegistry`：按文件聚合、按 severity 排序
//! （Error < Warning < Info < Hint），同一文件的新推送整体替换旧集。

use std::collections::HashMap;
use std::sync::RwLock;

use lsp_types::{Diagnostic, PublishDiagnosticsParams};

/// 单条诊断（含来源服务器与文件路径）。
#[derive(Clone, Debug)]
pub struct DiagnosticEntry {
    pub server: String,
    pub path: String,
    pub diagnostic: Diagnostic,
}

/// severity 排序权重（Error=1 < Warning=2 < Info=3 < Hint=4，未知排最后）。
fn severity_weight(diagnostic: &Diagnostic) -> u8 {
    match diagnostic.severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => 1,
        Some(lsp_types::DiagnosticSeverity::WARNING) => 2,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => 3,
        Some(lsp_types::DiagnosticSeverity::HINT) => 4,
        _ => 5,
    }
}

/// 诊断聚合注册表（线程安全；管理器与工具侧共享）。
pub struct DiagnosticRegistry {
    /// file path → (server name → diagnostics)
    by_file: RwLock<HashMap<String, HashMap<String, Vec<Diagnostic>>>>,
}

impl DiagnosticRegistry {
    pub fn new() -> Self {
        Self {
            by_file: RwLock::new(HashMap::new()),
        }
    }

    /// 处理一次 `publishDiagnostics` 推送（整体替换该 server 在该文件的旧集）。
    pub fn update(&self, server: &str, params: &PublishDiagnosticsParams) {
        let Ok(path) = super::uri_to_path(&params.uri) else {
            return;
        };
        let path = path.display().to_string();
        if let Ok(mut by_file) = self.by_file.write() {
            by_file
                .entry(path)
                .or_default()
                .insert(server.to_string(), params.diagnostics.clone());
        }
    }

    /// 清空某文件的诊断（didClose 时调用，避免陈旧错误悬挂）。
    pub fn clear_file(&self, server: &str, path: &str) {
        if let Ok(mut by_file) = self.by_file.write()
            && let Some(servers) = by_file.get_mut(path)
        {
            servers.remove(server);
        }
    }

    /// 某文件的全部诊断，按 severity → 行号 → 列号 排序。
    pub fn diagnostics_for(&self, path: &str) -> Vec<DiagnosticEntry> {
        let Ok(by_file) = self.by_file.read() else {
            return Vec::new();
        };
        let Some(servers) = by_file.get(path) else {
            return Vec::new();
        };
        let mut out: Vec<DiagnosticEntry> = servers
            .iter()
            .flat_map(|(server, diagnostics)| {
                diagnostics.iter().map(move |diagnostic| DiagnosticEntry {
                    server: server.clone(),
                    path: path.to_string(),
                    diagnostic: diagnostic.clone(),
                })
            })
            .collect();
        out.sort_by_key(|entry| {
            (
                severity_weight(&entry.diagnostic),
                entry.diagnostic.range.start.line,
                entry.diagnostic.range.start.character,
            )
        });
        out
    }

    /// 全项目全部诊断（按 severity → 文件 → 行号 排序）。
    pub fn all(&self) -> Vec<DiagnosticEntry> {
        let Ok(by_file) = self.by_file.read() else {
            return Vec::new();
        };
        let mut out: Vec<DiagnosticEntry> = by_file
            .iter()
            .flat_map(|(path, servers)| {
                servers.iter().flat_map(move |(server, diagnostics)| {
                    diagnostics.iter().map(move |diagnostic| DiagnosticEntry {
                        server: server.clone(),
                        path: path.clone(),
                        diagnostic: diagnostic.clone(),
                    })
                })
            })
            .collect();
        out.sort_by(|a, b| {
            severity_weight(&a.diagnostic)
                .cmp(&severity_weight(&b.diagnostic))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| {
                    a.diagnostic
                        .range
                        .start
                        .line
                        .cmp(&b.diagnostic.range.start.line)
                })
        });
        out
    }

    /// 某文件是否有 error 级诊断（编辑后即时反馈用）。
    pub fn has_errors(&self, path: &str) -> bool {
        self.diagnostics_for(path)
            .iter()
            .any(|entry| entry.diagnostic.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
    }

    /// 清空所有状态（会话切换时）。
    pub fn clear(&self) {
        if let Ok(mut by_file) = self.by_file.write() {
            by_file.clear();
        }
    }
}

impl Default for DiagnosticRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 把诊断格式化为单行摘要（工具输出用）。
pub fn format_diagnostic(path: &str, diagnostic: &Diagnostic) -> String {
    let severity = match diagnostic.severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
        Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
        Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
        _ => "diagnostic",
    };
    let line = diagnostic.range.start.line + 1;
    let column = diagnostic.range.start.character + 1;
    let source = diagnostic.source.as_deref().unwrap_or("lsp");
    format!(
        "{path}:{line}:{column}: {severity} [{source}]: {}",
        diagnostic.message.replace('\n', " ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{DiagnosticSeverity, Position, Range};

    fn uri(path: &str) -> lsp_types::Uri {
        super::super::path_to_uri(std::path::Path::new(path)).unwrap()
    }

    fn diagnostic(severity: Option<DiagnosticSeverity>, line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity,
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn update_replaces_previous_set_per_server() {
        let registry = DiagnosticRegistry::new();
        let file = "/tmp/x.rs";
        registry.update(
            "rust-analyzer",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![diagnostic(
                    Some(lsp_types::DiagnosticSeverity::ERROR),
                    1,
                    "old",
                )],
                version: None,
            },
        );
        registry.update(
            "rust-analyzer",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![],
                version: None,
            },
        );
        assert!(
            registry.diagnostics_for(file).is_empty(),
            "新推送应整体替换旧集"
        );
    }

    #[test]
    fn diagnostics_sorted_by_severity() {
        let registry = DiagnosticRegistry::new();
        let file = "/tmp/y.rs";
        registry.update(
            "srv",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![
                    diagnostic(Some(lsp_types::DiagnosticSeverity::WARNING), 3, "w"),
                    diagnostic(Some(lsp_types::DiagnosticSeverity::ERROR), 9, "e"),
                    diagnostic(Some(lsp_types::DiagnosticSeverity::HINT), 1, "h"),
                ],
                version: None,
            },
        );
        let entries = registry.diagnostics_for(file);
        let messages: Vec<&str> = entries
            .iter()
            .map(|entry| entry.diagnostic.message.as_str())
            .collect();
        assert_eq!(messages, vec!["e", "w", "h"]);
    }

    #[test]
    fn multiple_servers_merge_for_same_file() {
        let registry = DiagnosticRegistry::new();
        let file = "/tmp/z.rs";
        registry.update(
            "srv-a",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![diagnostic(
                    Some(lsp_types::DiagnosticSeverity::ERROR),
                    1,
                    "a",
                )],
                version: None,
            },
        );
        registry.update(
            "srv-b",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![diagnostic(
                    Some(lsp_types::DiagnosticSeverity::WARNING),
                    2,
                    "b",
                )],
                version: None,
            },
        );
        assert_eq!(registry.diagnostics_for(file).len(), 2);
        assert!(registry.has_errors(file));
    }

    #[test]
    fn clear_file_removes_only_that_server() {
        let registry = DiagnosticRegistry::new();
        let file = "/tmp/w.rs";
        registry.update(
            "srv-a",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![diagnostic(
                    Some(lsp_types::DiagnosticSeverity::ERROR),
                    1,
                    "a",
                )],
                version: None,
            },
        );
        registry.update(
            "srv-b",
            &PublishDiagnosticsParams {
                uri: uri(file),
                diagnostics: vec![diagnostic(
                    Some(lsp_types::DiagnosticSeverity::WARNING),
                    2,
                    "b",
                )],
                version: None,
            },
        );
        registry.clear_file("srv-a", file);
        let entries = registry.diagnostics_for(file);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].server, "srv-b");
    }
}
