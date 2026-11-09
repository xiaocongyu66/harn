//! 极简 HTTP/1.1 请求解析与路由（自研）。
//!
//! 只处理 Web UI 需要的 GET/HEAD 请求：
//! - `/` 与静态资源 → 内嵌 dist（路径遍历 403、缺失 404、非 GET/HEAD 405）；
//! - `/ws` → WebSocket 升级（须带 token）；
//! - `/health` → 200（本地探活，免鉴权）。
//!
//! 不引入 HTTP 框架：请求体一律不读取（WS 握手与静态资源都是 GET），
//! 保持解析器在 100 行以内。

use std::collections::HashMap;

/// 解析出的请求：方法、路径（含 query）、头部表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// 原始路径（含 `?query`）。
    pub target: String,
    /// 全小写头部名 → 值（原值大小写保留）。
    pub headers: HashMap<String, String>,
}

impl Request {
    /// 不含 query 的纯路径。
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }

    /// `?token=…` 取值。
    pub fn query_token(&self) -> Option<&str> {
        let (_, query) = self.target.split_once('?')?;
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=')?;
            if name == "token" && !value.is_empty() {
                return Some(value);
            }
        }
        None
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// RFC 6455 升级探测：`Upgrade: websocket` + `Connection: Upgrade`。
pub fn is_websocket_upgrade(request: &Request) -> bool {
    let upgrade = request
        .header("upgrade")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    let connection = request
        .header("connection")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    upgrade.contains("websocket") && connection.contains("upgrade")
}

/// 从字节流头部解析请求；`env` 为已读到的完整头部文本（含末尾空行）。
pub fn parse_request(head: &str) -> Result<Request, ParseError> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(ParseError::Malformed)?.to_string();
    let target = parts.next().ok_or(ParseError::Malformed)?.to_string();
    match parts.next() {
        // 版本必须形如 `HTTP/1.1`。
        Some(version) if version.starts_with("HTTP/") => {}
        _ => return Err(ParseError::Malformed),
    }
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(ParseError::Malformed)?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(Request {
        method,
        target,
        headers,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("请求畸形")]
    Malformed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(head: &str) -> Request {
        parse_request(head).unwrap()
    }

    #[test]
    fn parses_method_path_and_headers() {
        let request = req(
            "GET /ws?token=abc123 HTTP/1.1\r\nHost: 127.0.0.1:7331\r\nUpgrade: websocket\r\n\r\n",
        );
        assert_eq!(request.method, "GET");
        assert_eq!(request.path(), "/ws");
        assert_eq!(request.query_token(), Some("abc123"));
        assert_eq!(request.header("host"), Some("127.0.0.1:7331"));
        assert_eq!(request.header("HOST"), Some("127.0.0.1:7331"));
    }

    #[test]
    fn path_strips_query() {
        let request = req("GET /assets/main.js?v=42 HTTP/1.1\r\n\r\n");
        assert_eq!(request.path(), "/assets/main.js");
        assert_eq!(request.query_token(), None);
    }

    #[test]
    fn detects_websocket_upgrade() {
        let request = req("GET /ws HTTP/1.1\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\n\r\n");
        assert!(is_websocket_upgrade(&request));
        let plain = req("GET / HTTP/1.1\r\n\r\n");
        assert!(!is_websocket_upgrade(&plain));
    }

    #[test]
    fn malformed_requests_rejected() {
        for head in ["", "GET\r\n\r\n", "GET / HTTP\r\n", "GET /\r\n\r\n"] {
            assert!(parse_request(head).is_err(), "{head:?}");
        }
    }

    #[test]
    fn empty_token_rejected() {
        let request = req("GET /ws?token= HTTP/1.1\r\n\r\n");
        assert_eq!(request.query_token(), None);
    }
}
