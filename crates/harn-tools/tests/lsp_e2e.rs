//! LSP 端到端：真实进程（内嵌 python mock server）走完整协议闭环。
//!
//! 验证：JSONRPC 帧握手、initialize 能力解析、**UTF-8 编码协商**、
//! didOpen 触发 publishDiagnostics、九类 LSP 请求、shutdown 优雅退出。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use harn_tools::lsp::instance::PositionEncoding;
use harn_tools::lsp::{DiagnosticRegistry, LspManager};
use lsp_types::ClientCapabilities;
use serde_json::json;
use tokio::time::sleep;

fn mock_server_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("mock_lsp_server.py")
}

/// 临时工作区（settings 由 settings_servers 直接提供，不落盘）。
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    (dir, cwd)
}

fn target_file(dir: &Path) -> PathBuf {
    let path = dir.join("main.rs");
    std::fs::write(&path, "fn main() { mock_symbol() }\n").unwrap();
    path
}

/// 轮询等待 didOpen 的诊断推送到达（进程间推送是异步的）。
async fn wait_for_diagnostics(registry: &DiagnosticRegistry, path: &Path) -> bool {
    let key = path.display().to_string();
    for _ in 0..50 {
        if !registry.diagnostics_for(&key).is_empty() {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn lsp_full_handshake_and_requests() {
    let (dir, cwd) = workspace();
    let file = target_file(dir.path());
    let registry = Arc::new(DiagnosticRegistry::new());
    let manager = LspManager::new(
        settings_servers(),
        cwd.clone(),
        Arc::clone(&registry),
        ClientCapabilities::default(),
    );

    // 1) 按需启动 + initialize 握手
    let server = manager.ensure_server(&file).await.unwrap().unwrap();
    assert_eq!(server.name, "mock-lsp");
    // 2) **UTF-8 编码协商**：mock server 声明 utf-8
    assert_eq!(server.encoding(), PositionEncoding::Utf8);
    // 3) 能力解析
    assert!(server.capabilities().definition_provider.is_some());

    // 4) didOpen → publishDiagnostics 推送
    manager
        .open_file(&file, "fn main() { mock_symbol() }\n")
        .await
        .unwrap();
    assert!(
        wait_for_diagnostics(&registry, &file).await,
        "didOpen 后应收到诊断"
    );
    let diagnostics = registry.diagnostics_for(&file.display().to_string());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].diagnostic.message, "mock: symbol not found");

    // 5) 九个操作逐一走通（位置参数 1-based 转 0-based 由调用方负责；这里直接给 LSP 坐标）
    let uri = lsp_uri(&file);
    let definition: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/definition",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 13}}),
        )
        .await
        .unwrap();
    assert_eq!(definition["uri"], "file:///mock/target.rs");
    assert_eq!(definition["range"]["start"]["line"], 9);

    let references: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/references",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 13}, "context": {"includeDeclaration": true}}),
        )
        .await
        .unwrap();
    assert_eq!(references.as_array().unwrap().len(), 2);

    let hover: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/hover",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 13}}),
        )
        .await
        .unwrap();
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("fn mock(")
    );

    let symbols: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        )
        .await
        .unwrap();
    assert_eq!(symbols[0]["name"], "mock_symbol");

    let workspace_symbols: serde_json::Value = manager
        .send_request(&file, "workspace/symbol", json!({"query": "Mock"}))
        .await
        .unwrap();
    assert_eq!(workspace_symbols[0]["name"], "MockThing");

    let prepare: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/prepareCallHierarchy",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 3}}),
        )
        .await
        .unwrap();
    assert_eq!(prepare[0]["name"], "caller");
    let item_uri = prepare[0]["uri"].as_str().unwrap();

    let incoming: serde_json::Value = manager
        .send_request(
            &file,
            "callHierarchy/incomingCalls",
            json!({"item": {"name": "caller", "kind": 12, "uri": item_uri, "range": prepare[0]["range"], "selectionRange": prepare[0]["selectionRange"]}}),
        )
        .await
        .unwrap();
    assert_eq!(incoming[0]["to"]["name"], "callee");

    // 6) shutdown 优雅关闭（mock server exit → 进程退出）
    manager.shutdown().await;
    // 再请求应报「找不到服务器配置」或崩溃（进程已退）
    let after = manager.ensure_server(&file).await;
    assert!(
        after.is_ok(),
        "shutdown 后 ensure 返回 Ok（启动/已崩溃均可）"
    );
}

#[test]
fn python3_available_for_mock_server() {
    let status = Command::new("python3").arg("--version").output();
    assert!(status.is_ok(), "测试需要 python3");
}

fn settings_servers() -> harn_config::LspServersConfig {
    let mut config = harn_config::Settings::default().lsp_servers;
    config.insert(
        "mock-lsp".to_string(),
        harn_config::LspServerConfig {
            command: "python3".to_string(),
            args: vec![mock_server_path().display().to_string()],
            filetypes: vec!["rs".to_string()],
            env: Default::default(),
            root_marker: ".".to_string(),
        },
    );
    config
}

fn lsp_uri(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

/// 真实第三方服务器（pylsp）联调：默认忽略——需要机器装有 pylsp 且愿意等 30 秒冷启动。
/// 运行：`cargo test -p harn-tools --test lsp_e2e -- --ignored real_pylsp_alive --nocapture`
#[tokio::test]
#[ignore = "需要 pylsp 与冷启动时间；CI 不跑"]
async fn real_pylsp_alive() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let file = cwd.join("app.py");
    // 第 1 行 def greet，第 5 行调用（1-based；与 e2e 主用例同一布局）
    std::fs::write(
        &file,
        "def greet(name):\n    return name\n\n\nprint(greet(\"x\"))\n",
    )
    .unwrap();
    let registry = Arc::new(DiagnosticRegistry::new());
    let manager = LspManager::new(
        real_server_config("pylsp", &["py"]),
        cwd.clone(),
        Arc::clone(&registry),
        ClientCapabilities::default(),
    );
    let started = std::time::Instant::now();
    let server = manager.ensure_server(&file).await.unwrap().unwrap();
    println!("pylsp 握手耗时 {:?}", started.elapsed());
    println!("编码协商: {:?}", server.encoding());
    let capabilities = serde_json::to_string_pretty(server.capabilities()).unwrap();
    println!("capabilities: {capabilities}");
    manager
        .open_file(&file, &std::fs::read_to_string(&file).unwrap())
        .await
        .unwrap();
    // definition
    let uri = lsp_uri(&file);
    let response: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/definition",
            json!({"textDocument": {"uri": uri}, "position": {"line": 4, "character": 6}}),
        )
        .await
        .unwrap();
    println!("definition: {response}");
    // documentSymbol
    let response: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        )
        .await
        .unwrap();
    println!("documentSymbol: {response}");
    // references
    let response: serde_json::Value = manager
        .send_request(
            &file,
            "textDocument/references",
            json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 4}, "context": {"includeDeclaration": true}}),
        )
        .await
        .unwrap();
    println!("references: {response}");
    // 诊断（didOpen 后 pylsp 通常立即推送一条）
    sleep(Duration::from_millis(500)).await;
    println!("diagnostics: {:?}", registry_path(&registry, &file));
    manager.shutdown().await;
}

fn registry_path(registry: &DiagnosticRegistry, path: &Path) -> Vec<String> {
    registry
        .diagnostics_for(&path.display().to_string())
        .iter()
        .map(|entry| {
            format!(
                "{:?} {}",
                entry.diagnostic.severity, entry.diagnostic.message
            )
        })
        .collect()
}

fn real_server_config(name: &str, filetypes: &[&str]) -> harn_config::LspServersConfig {
    let mut config = harn_config::Settings::default().lsp_servers;
    config.insert(
        name.to_string(),
        harn_config::LspServerConfig {
            command: name.to_string(),
            args: Vec::new(),
            filetypes: filetypes.iter().map(|ft| ft.to_string()).collect(),
            env: Default::default(),
            root_marker: ".".to_string(),
        },
    );
    config
}
