//! 集成测试辅助文件：helper 断言失败即 panic 是预期行为。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use harn_core::event::EventSink;
use harn_core::message::{ContentBlock, Message};
use harn_core::provider::{Provider, ProviderRequest};
use harn_providers::{OpenAiApi, OpenAiProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve_once(sse: String) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        let header_end = loop {
            let mut chunk = [0u8; 2048];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = find(&buffer, b"\r\n\r\n") {
                break Some(position + 4);
            }
        };
        let Some(header_end) = header_end else {
            return;
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let mut chunk = [0u8; 2048];
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&buffer[header_end..]).to_string();
        let _ = tx.send((headers, body));

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            sse.len(),
            sse
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}/v1"), rx)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.to_string(),
        system: "system".to_string(),
        messages: Arc::new(vec![Message::user_text("hello")]),
        tools: Vec::new(),
        max_tokens: 64,
        agent_id: "test".to_string(),
    }
}

#[tokio::test]
async fn chat_provider_streams_over_http() {
    let sse = [
        r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"hel"},"finish_reason":null}]}"#,
        "",
        r#"data: {"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
        "",
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "",
        r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#,
        "",
        "data: [DONE]",
        "",
    ]
    .join("\n");
    let (base, body_rx) = serve_once(sse).await;
    let provider = OpenAiProvider::new(OpenAiApi::Chat, "test-key", base).unwrap();
    let turn = provider
        .complete(request("gpt-test"), &EventSink::noop())
        .await
        .unwrap();
    assert_eq!(turn.usage.input_tokens, 5);
    assert_eq!(turn.usage.output_tokens, 3);
    assert_eq!(turn.text(), "hello");
    assert!(matches!(
        turn.content.last(),
        Some(ContentBlock::ToolUse { name, .. }) if name == "Read"
    ));

    let (_headers, body) = tokio::time::timeout(Duration::from_secs(2), body_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(body.contains("\"messages\""));
    assert!(body.contains("\"stream\":true"));
    assert!(body.contains("\"content\":\"hello\""));
}

#[tokio::test]
async fn responses_provider_streams_over_http() {
    let sse = [
        r#"data: {"type":"response.output_text.delta","delta":"hello "}"#,
        "",
        r#"data: {"type":"response.output_text.delta","delta":"world"}"#,
        "",
        r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_7","name":"Grep","arguments":""}}"#,
        "",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"pattern\":\"TODO\"}"}"#,
        "",
        r#"data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":9,"output_tokens":4},"output":[{"type":"message","content":[{"type":"output_text","text":"hello world"}]},{"type":"function_call","id":"fc_1","call_id":"call_7","name":"Grep","arguments":"{\"pattern\":\"TODO\"}"}]}}"#,
        "",
    ]
    .join("\n");
    let (base, body_rx) = serve_once(sse).await;
    let provider = OpenAiProvider::new(OpenAiApi::Responses, "test-key", base).unwrap();
    let turn = provider
        .complete(request("gpt-test"), &EventSink::noop())
        .await
        .unwrap();
    assert_eq!(turn.usage.input_tokens, 9);
    assert_eq!(turn.usage.output_tokens, 4);
    assert_eq!(turn.text(), "hello world");
    assert!(matches!(
        turn.content.last(),
        Some(ContentBlock::ToolUse { name, input, .. })
            if name == "Grep" && input["pattern"] == "TODO"
    ));

    let (_headers, body) = tokio::time::timeout(Duration::from_secs(2), body_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(body.contains("\"instructions\""));
    assert!(body.contains("\"input\""));
    assert!(body.contains("\"store\":false"));
}

#[tokio::test]
async fn requests_carry_unified_user_agent() {
    let (base, rx) = serve_once("data: [DONE]\n\n".to_string()).await;
    let provider = OpenAiProvider::new(OpenAiApi::Chat, "test-key", base).unwrap();
    let _ = provider
        .complete(request("gpt-test"), &EventSink::noop())
        .await;
    let (headers, _) = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        headers.lines().any(|line| line
            .to_ascii_lowercase()
            .starts_with("user-agent: harn-cli/")),
        "应在请求头中看到统一 UA：{headers}"
    );
}
