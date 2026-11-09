//! Web UI 前后端共享协议（wasm-clean：serde-only，无 tokio/std::process）。
//!
//! 设计原则：
//! - **方向明确**：`ClientMsg`（浏览器→harn）/ `ServerMsg`（harn→浏览器）；
//! - **复用核心语义**：DTO 与 harn-ui 的 Entry/UiAsk 同构，native 侧做转换；
//! - **流式与离散分离**：增量 token 走 `Delta`，完整条目走 `Entry`；
//! - **Modal 双向**：harn UiAsk 弹窗 → `Modal`；浏览器回复 → `Choose`。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 浏览器 → harn
// ---------------------------------------------------------------------------

/// 浏览器发出的消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 发送一条用户消息（开启新一轮 agent loop）。
    Send { text: String },
    /// 中断当前 turn（TUI Ctrl-C 等价）。
    Abort,
    /// 回答 harn 的弹窗（权限审批 / Question）。
    Choose { reply: ModalReply },
    /// 切换权限模式。
    SetMode { mode: String },
    /// 切换界面语言。
    SetLocale { locale: String },
}

/// 弹窗回复：与 TUI 的 PermissionChoice/Question 回复同构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModalReply {
    /// 权限：允许一次 / 总是允许 / 拒绝。
    Permission { choice: PermissionChoiceDto },
    /// 提问：选项 label 列表（单选一条/多选多条，None = Dismiss）。
    Question { answers: Option<Vec<String>> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionChoiceDto {
    AllowOnce,
    AllowAlways,
    Deny,
}

// ---------------------------------------------------------------------------
// harn → 浏览器
// ---------------------------------------------------------------------------

/// harn 推给浏览器的消息。
///
/// 内层 DTO（Entry/Modal/Status）各自带 `kind` 标签，所以外层用**命名字段**
/// 承载 payload——内外共用 `"kind"` 会序列化出重复键，反序列化按后者解析
/// 导致往返错位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 一条完整条目入流（用户消息/助手文本/工具块/状态行/错误）。
    Entry { entry: EntryDto },
    /// 流式 token 增量（当前助手条目追加，按 agent_id 关联）。
    Delta { agent_id: String, text: String },
    /// harn 请求弹窗（权限审批 / Question）。
    Modal { modal: ModalDto },
    /// 状态栏快照。
    Status { status: StatusDto },
    /// 当前 turn 结束（含终止原因）。
    Done { reason: String },
    /// 协议/桥接层错误（非 agent 错误——那走 Entry）。
    Error { message: String },
}

/// 流条目：与 harn-ui transcript 的 Entry 同构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryDto {
    /// 用户输入（含附件数）。
    User { text: String, images: u32 },
    /// 助手文本块（可含 markdown）。
    Assistant { agent_id: String, text: String },
    /// 助手思考/推理块。
    Thinking { agent_id: String, text: String },
    /// 工具调用块。
    Tool {
        agent_id: String,
        name: String,
        input: serde_json::Value,
        status: ToolStatusDto,
    },
    /// 工具结果。
    ToolResult {
        agent_id: String,
        name: String,
        output: String,
        is_error: bool,
        ms: u64,
    },
    /// 状态/提示行（模式切换、权限 mode 变更等）。
    Status { text: String },
    /// 错误条目。
    Error { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatusDto {
    Running,
    Ok,
    Error,
}

/// 弹窗请求：与 harn 的 UiAsk 同构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModalDto {
    /// 权限审批。
    Permission {
        tool: String,
        target: Option<String>,
        reason: String,
    },
    /// 提问。
    Question {
        question: String,
        header: String,
        options: Vec<QuestionOptionDto>,
        multi_select: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOptionDto {
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
}

/// 状态栏：模式/model/token 用量/运行态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDto {
    pub mode: String,
    pub model: String,
    pub provider: String,
    pub running: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_server_messages_roundtrip() {
        let cases: Vec<ClientMsg> = vec![
            ClientMsg::Send {
                text: "写个脚本".into(),
            },
            ClientMsg::Abort,
            ClientMsg::Choose {
                reply: ModalReply::Permission {
                    choice: PermissionChoiceDto::AllowOnce,
                },
            },
            ClientMsg::Choose {
                reply: ModalReply::Question {
                    answers: Some(vec!["staging".into()]),
                },
            },
            ClientMsg::SetMode {
                mode: "acceptEdits".into(),
            },
            ClientMsg::SetLocale {
                locale: "en".into(),
            },
        ];
        for msg in cases {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back, "{json}");
        }

        let server: Vec<ServerMsg> = vec![
            ServerMsg::Entry {
                entry: EntryDto::User {
                    text: "hi".into(),
                    images: 0,
                },
            },
            ServerMsg::Delta {
                agent_id: "a1".into(),
                text: "hello".into(),
            },
            ServerMsg::Entry {
                entry: EntryDto::Tool {
                    agent_id: "a1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "a.txt"}),
                    status: ToolStatusDto::Ok,
                },
            },
            ServerMsg::Entry {
                entry: EntryDto::ToolResult {
                    agent_id: "a1".into(),
                    name: "Read".into(),
                    output: "hello".into(),
                    is_error: false,
                    ms: 2,
                },
            },
            ServerMsg::Modal {
                modal: ModalDto::Permission {
                    tool: "Write".into(),
                    target: Some("note.txt".into()),
                    reason: "默认模式需要确认".into(),
                },
            },
            ServerMsg::Status {
                status: StatusDto {
                    mode: "default".into(),
                    model: "GLM-5.3-Flash".into(),
                    provider: "openai-chat".into(),
                    running: true,
                    input_tokens: 5,
                    output_tokens: 3,
                },
            },
            ServerMsg::Done {
                reason: "end_turn".into(),
            },
            ServerMsg::Error {
                message: "boom".into(),
            },
        ];
        for msg in server {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ServerMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back, "{json}");
        }
    }

    #[test]
    fn tag_names_are_snake_case() {
        // 前端靠 tag 字段分发，形态必须稳定可读。
        let json = serde_json::to_string(&ClientMsg::Abort).unwrap();
        assert_eq!(json, "{\"kind\":\"abort\"}");
        let json = serde_json::to_string(&ServerMsg::Entry {
            entry: EntryDto::Status { text: "x".into() },
        })
        .unwrap();
        assert!(json.contains("\"kind\":\"entry\""));
        assert!(json.contains("\"kind\":\"status\""));
    }
}
