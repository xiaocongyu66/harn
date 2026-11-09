//! WebFetch 工具：抓取 URL 并转成纯文本（移植 Claude Code 的 WebFetch 语义，
//! 用 reqwest 而非浏览器；内置 SSRF 防护）。

use std::time::Duration;

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput, object_schema};

const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHARS: usize = 30_000;

/// 拒绝内网/环回/链路本地地址（SSRF 防护）。
fn is_blocked_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    // reqwest 的 host_str 对 IPv6 返回带方括号形式 `[::1]`。
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host.split(':').next().unwrap_or_default());
    if host == "localhost" || host.ends_with(".localhost") || host.is_empty() {
        return true;
    }
    let Ok(address) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match address {
        std::net::IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_broadcast()
                || ipv4.is_unspecified()
                || ipv4.is_documentation()
        }
        std::net::IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || (ipv6.segments()[0] & 0xfe00) == 0xfc00
                || (ipv6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// 极简 HTML → 文本（去 script/style/注释、剥标签、实体解码、压缩空行）。
fn html_to_text(html: &str) -> String {
    let mut stripped = String::with_capacity(html.len() / 2);
    let mut rest = html;
    // 去 script/style：按最早出现位置统一处理，只累积段前内容。
    loop {
        let lower = rest.to_ascii_lowercase();
        let script = lower.find("<script");
        let style = lower.find("<style");
        let start = match (script, style) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let Some(start) = start else { break };
        let close = if script == Some(start) {
            "</script>"
        } else {
            "</style>"
        };
        stripped.push_str(&rest[..start]);
        match rest[start..].to_ascii_lowercase().find(close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    stripped.push_str(rest);
    let mut text = stripped;

    // 去注释
    while let Some(start) = text.find("<!--") {
        let Some(end) = text[start..].find("-->") else {
            break;
        };
        text.replace_range(start..start + end + 3, " ");
    }

    // 剥标签并按块级元素换行
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    // 实体解码
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");

    // 压缩空白
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a public http(s) URL and return its content as text. Use to read documentation, \
         API references, or issue pages. Local/private network addresses are refused."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "url": {"type": "string", "description": "Absolute http(s) URL."},
                "maxChars": {
                    "type": "integer",
                    "description": "Trim result to this many characters (default 30000)."
                }
            }),
            &["url"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.webfetch.url_required")))?
            .to_string();
        let max_chars = input
            .get("maxChars")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(MAX_CHARS)
            .clamp(500, MAX_CHARS);

        let parsed = reqwest::Url::parse(&url).map_err(|error| {
            HarnError::Tool(harn_i18n::tr!("tool.webfetch.url_invalid", error = error))
        })?;
        match parsed.scheme() {
            "http" | "https" => {}
            other => {
                return Ok(ToolOutput::error(harn_i18n::tr!(
                    "tool.webfetch.scheme_unsupported",
                    scheme = other
                )));
            }
        }
        let host = parsed.host_str().unwrap_or_default();
        if is_blocked_host(host) {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.webfetch.ssrf_blocked",
                host = host
            )));
        }

        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("harn/0.1 (+https://example.invalid)")
            .build()
            .map_err(|error| {
                HarnError::Tool(harn_i18n::tr!(
                    "tool.webfetch.client_build_failed",
                    error = error
                ))
            })?;
        let response = client.get(parsed).send().await.map_err(|error| {
            HarnError::Tool(harn_i18n::tr!(
                "tool.webfetch.request_failed",
                error = error
            ))
        })?;
        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.webfetch.http_status",
                status = status
            )));
        }
        let Some(content_type) = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
        else {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.webfetch.missing_content_type"
            )));
        };
        if !content_type.starts_with("text/")
            && !content_type.contains("html")
            && !content_type.contains("json")
            && !content_type.contains("xml")
        {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.webfetch.content_type_unsupported",
                content_type = content_type
            )));
        }
        let bytes = response.bytes().await.map_err(|error| {
            HarnError::Tool(harn_i18n::tr!("tool.webfetch.read_failed", error = error))
        })?;
        if bytes.len() > MAX_BYTES {
            return Ok(ToolOutput::error(harn_i18n::tr!(
                "tool.webfetch.too_large",
                max_bytes = MAX_BYTES
            )));
        }
        let body = String::from_utf8_lossy(&bytes).to_string();
        let text = if content_type.contains("html") {
            html_to_text(&body)
        } else {
            body
        };
        let text: String = text.chars().take(max_chars).collect();
        let truncated = text.chars().count() >= max_chars;
        ctx.events.emit(harn_core::event::AgentEvent::Status {
            agent_id: ctx.agent_id.clone(),
            text: format!("fetched {url} ({} chars)", text.chars().count()),
        });
        Ok(ToolOutput::ok(format!(
            "URL: {url}\nContent-Type: {content_type}\n\n{text}{}",
            if truncated { "\n[...truncated]" } else { "" }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_hosts() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("127.0.0.1"));
        assert!(is_blocked_host("10.1.2.3"));
        assert!(is_blocked_host("192.168.0.1"));
        assert!(is_blocked_host("169.254.169.254"));
        assert!(is_blocked_host("[::1]"));
        assert!(!is_blocked_host("example.com"));
        assert!(!is_blocked_host("93.184.216.34"));
    }

    #[test]
    fn strips_tags_and_scripts() {
        let html = "<html><head><style>p{color:red}</style></head>\
                    <body><h1>Title</h1><p>Hello&nbsp;world &lt;tag&gt;</p>\
                    <script>alert(1)</script><div>a<br>b</div></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world <tag>"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("alert(1)"));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let out = WebFetchTool
            .call(
                json!({"url": "ftp://example.com/x"}),
                &crate::ToolContext::for_test(std::env::temp_dir()),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains(&harn_i18n::tr!(
            "tool.webfetch.scheme_unsupported",
            scheme = "ftp"
        )));
    }

    #[tokio::test]
    async fn rejects_loopback_url() {
        let out = WebFetchTool
            .call(
                json!({"url": "http://127.0.0.1:8080/admin"}),
                &crate::ToolContext::for_test(std::env::temp_dir()),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains(&harn_i18n::tr!(
            "tool.webfetch.ssrf_blocked",
            host = "127.0.0.1"
        )));
    }
}
