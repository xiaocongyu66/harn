//! RFC 6455 WebSocket 服务端最小实现（自研，对齐项目去依赖路线）。
//!
//! 只实现 harn Web UI 需要的部分：
//! - 握手：`Sec-WebSocket-Accept` = base64(sha1(key + GUID))；
//! - 帧：解析 FIN/opcode/mask/长度变体（7/16/64 位），客户端→服务端必须带掩码；
//! - 控制帧：Ping/Pong/Close；文本帧承载 JSON 协议消息；
//! - 不实现：分片消息重组（协议层消息小且整帧发送）、压缩扩展。
//!
//! 安全要点：掩码解在固定缓冲里；负载长度上限 `MAX_PAYLOAD` 防内存放大。

use base64::Engine;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 握手 GUID（RFC 6455 §1.3）。
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 单帧负载上限（8 MiB）：协议消息是 JSON 文本，远小于此。
pub const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// 计算 `Sec-WebSocket-Accept` 握手响应值。
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// 检查 `Sec-WebSocket-Key` 是否为 16 字节 base64 形态（长度 24、末尾 `==`）。
pub fn is_valid_client_key(key: &str) -> bool {
    key.len() == 24 && key.ends_with("==") && key.is_ascii()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl Opcode {
    fn from(byte: u8) -> Option<Self> {
        match byte {
            0x0 => Some(Opcode::Continuation),
            0x1 => Some(Opcode::Text),
            0x2 => Some(Opcode::Binary),
            0x8 => Some(Opcode::Close),
            0x9 => Some(Opcode::Ping),
            0xA => Some(Opcode::Pong),
            _ => None,
        }
    }
}

/// 解析后的 WebSocket 消息（控制帧与数据帧统一表示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>, String),
}

/// 从流中读取一帧并规范化为消息；返回 `None` 表示对端正常关闭。
///
/// 协议错误一律以 `Err` 上抛，由调用者直接断开连接（RFC 要求失败时
/// 关闭连接而非尝试恢复）。
pub async fn read_message<S>(stream: &mut S) -> Result<Option<Message>, WsError>
where
    S: AsyncRead + Unpin,
{
    let header = stream.read_u16().await?;
    let fin = header & 0x8000 != 0;
    let opcode = Opcode::from(((header >> 8) & 0x0F) as u8)
        .ok_or(WsError::BadOpcode((header >> 8) & 0x0F))?;
    let masked = header & 0x0080 != 0;
    let mut len = (header & 0x007F) as u64;
    if len == 126 {
        len = stream.read_u16().await? as u64;
    } else if len == 127 {
        len = stream.read_u64().await?;
        // RFC：64 位长度的高 32 位必须为零。
        if len >> 32 != 0 {
            return Err(WsError::Oversized);
        }
    }
    if len > MAX_PAYLOAD as u64 {
        return Err(WsError::Oversized);
    }
    let mask = if masked {
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).await?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    if !payload.is_empty() {
        stream.read_exact(&mut payload).await?;
    }
    if let Some(mask) = mask {
        apply_mask(&mut payload, &mask);
    } else if opcode != Opcode::Close {
        // 服务端只接受带掩码的客户端帧（RFC §5.1）；Close 也一律要求掩码，
        // 但放行无掩码的 Close 以容忍少数实现，随即关闭。
        return Err(WsError::UnmaskedClient);
    }

    if !fin {
        // 协议层消息一帧发完；分片仅在对端异常时出现，按协议失败处理。
        return Err(WsError::Fragmented);
    }
    Ok(match opcode {
        Opcode::Text => {
            let text = String::from_utf8(payload).map_err(|_| WsError::BadUtf8)?;
            Some(Message::Text(text))
        }
        Opcode::Binary => Some(Message::Binary(payload)),
        Opcode::Ping => Some(Message::Ping(payload)),
        Opcode::Pong => Some(Message::Pong(payload)),
        Opcode::Close => {
            let (code, reason) = if payload.len() >= 2 {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                let reason = String::from_utf8_lossy(&payload[2..]).to_string();
                (Some(code), reason)
            } else {
                (None, String::new())
            };
            Some(Message::Close(code, reason))
        }
        Opcode::Continuation => return Err(WsError::StrayContinuation),
    })
}

/// 写一帧（服务端发出，不加掩码）。
pub async fn write_message<S>(stream: &mut S, message: &Message) -> Result<(), WsError>
where
    S: AsyncWrite + Unpin,
{
    let (opcode, payload): (u8, &[u8]) = match message {
        Message::Text(text) => (Opcode::Text as u8, text.as_bytes()),
        Message::Binary(bytes) => (Opcode::Binary as u8, bytes),
        Message::Ping(bytes) => (Opcode::Ping as u8, bytes),
        Message::Pong(bytes) => (Opcode::Pong as u8, bytes),
        Message::Close(_, reason) => (Opcode::Close as u8, reason.as_bytes()),
    };
    let len = payload.len();
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | opcode); // FIN=1
    if len < 126 {
        header.push(len as u8);
    } else if len <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    stream.write_all(&header).await?;
    if !payload.is_empty() {
        stream.write_all(payload).await?;
    }
    stream.flush().await?;
    Ok(())
}

/// 生成掩码并应用到负载（客户端方向；服务端发出不需要，保留以便测试对称）。
pub fn apply_mask(payload: &mut [u8], mask: &[u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index & 3];
    }
}

/// 生成随机 16 字节客户端 key（base64，`Sec-WebSocket-Key` 形态）。
/// 集成测试扮演 WebSocket 客户端时使用。
#[allow(dead_code)]
pub fn random_client_key() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("io 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("非法 opcode：0x{0:X}")]
    BadOpcode(u16),
    #[error("负载超限（超过 8 MiB）")]
    Oversized,
    #[error("客户端帧未带掩码")]
    UnmaskedClient,
    #[error("不支持分片消息")]
    Fragmented,
    #[error("游离的 continuation 帧")]
    StrayContinuation,
    #[error("文本帧不是合法 UTF-8")]
    BadUtf8,
    #[error("HTTP 头部畸形")]
    BadFrame,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_example() {
        // RFC 6455 §4.2.2 的范例值。
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn client_key_is_24_char_base64() {
        let key = random_client_key();
        assert!(is_valid_client_key(&key), "{key}");
    }

    #[test]
    fn mask_round_trips() {
        let mut payload = b"Hello, WebSocket!".to_vec();
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        apply_mask(&mut payload, &mask);
        assert_ne!(payload, b"Hello, WebSocket!");
        apply_mask(&mut payload, &mask);
        assert_eq!(payload, b"Hello, WebSocket!");
    }

    /// 帧往返：把消息编码进内存流再读回来。
    #[tokio::test]
    async fn write_then_read_roundtrips() {
        // write_message 不加掩码，read_message 要求客户端掩码——用
        // mask_handcrafted 直接造带掩码的字节流验证读路径。
        let cases = [
            (Opcode::Text as u8, "hello".as_bytes().to_vec()),
            (Opcode::Ping as u8, vec![1, 2, 3]),
            (Opcode::Binary as u8, vec![0xf4, 0x8a]),
        ];
        for (opcode, payload) in cases {
            let masked_payload: Vec<u8> = payload.iter().map(|b| b ^ 0x55).collect();
            let mut frame = vec![0x80 | opcode];
            frame.push(0x80 | payload.len() as u8); // mask=1
            frame.extend_from_slice(&[0x55; 4]);
            frame.extend_from_slice(&masked_payload);
            let mut cursor = std::io::Cursor::new(frame);
            let message = read_message(&mut cursor).await.unwrap().unwrap();
            let expected = match opcode {
                0x1 => Message::Text(String::from_utf8(payload).unwrap()),
                0x2 => Message::Binary(payload),
                0x9 => Message::Ping(payload),
                _ => unreachable!(),
            };
            assert_eq!(message, expected);
        }
    }

    #[tokio::test]
    async fn unmasked_client_frame_is_rejected() {
        let frame = [0x81, 0x05, b'h', b'e', b'l', b'l', b'o'];
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_message(&mut cursor).await;
        assert!(matches!(result, Err(WsError::UnmaskedClient)));
    }

    #[tokio::test]
    async fn write_text_message_frame_bytes() {
        let mut out: Vec<u8> = Vec::new();
        write_message(&mut out, &Message::Text("hi".into()))
            .await
            .unwrap();
        // FIN+Text, len=2, 无掩码位
        assert_eq!(out, vec![0x81, 0x02, b'h', b'i']);
    }

    #[tokio::test]
    async fn extended_length_126() {
        // 130 字节文本：长度走 16 位扩展。
        let payload = "x".repeat(130);
        let mask = [0x01, 0x02, 0x03, 0x04];
        let masked: Vec<u8> = payload
            .bytes()
            .enumerate()
            .map(|(i, b)| b ^ mask[i & 3])
            .collect();
        let mut frame = vec![0x81, 0xFE];
        frame.extend_from_slice(&130u16.to_be_bytes());
        frame.extend_from_slice(&mask);
        frame.extend_from_slice(&masked);
        let mut cursor = std::io::Cursor::new(frame);
        let message = read_message(&mut cursor).await.unwrap().unwrap();
        assert_eq!(message, Message::Text(payload));
    }

    #[tokio::test]
    async fn oversized_payload_is_rejected() {
        // 声明 64 位长度超过上限（只看头部即拒，无需真实负载）。
        let mut frame = vec![0x81, 0x7F];
        frame.extend_from_slice(&((MAX_PAYLOAD as u64) + 1).to_be_bytes());
        frame.extend_from_slice(&[0u8; 4]); // 掩码占位，让解析走到长度校验后读失败前
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_message(&mut cursor).await;
        assert!(matches!(result, Err(WsError::Oversized)), "{result:?}");
    }
}
