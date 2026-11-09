//! 提问工具：Agent 通过 UI 向用户提问（TUI 弹层 / 交互式 REPL prompter）。
//!
//! 与权限弹层同构：工具只负责把问题送进 channel 并等待回答；
//! 未注册交互界面（非交互运行）时直接说明无法提问，让模型自行决策，
//! 而不是无限期挂起 Agent 循环。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use harn_core::error::{HarnError, Result};

use super::{QuestionChannel, Tool, ToolContext, ToolOutput, object_schema};

/// 等待用户回答的上限；超时视为「无法获得答案」，与取消同等处理。
pub const ASK_TIMEOUT: Duration = Duration::from_secs(600);

const MAX_QUESTIONS: usize = 4;
const MAX_OPTIONS: usize = 8;

#[derive(Clone, Debug)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
    /// 选中该项时展示给用户的预览（仅单选有意义；对齐 CC AskUserQuestion 的 preview）。
    pub preview: Option<String>,
}

/// 一次提问请求；`reply` 回传所选 label 列表（`None` 表示用户取消）。
pub struct UserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<String>>>,
}

fn parse_question(value: &Value) -> Result<(String, String, Vec<QuestionOption>, bool)> {
    let object = value
        .as_object()
        .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.ask.question_must_be_object")))?;
    let question = object
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.ask.question_required")))?
        .to_string();
    let header = object
        .get("header")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Question")
        .to_string();
    let options = object
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let label = item
                        .get("label")
                        .and_then(Value::as_str)
                        .or_else(|| Value::as_str(item))
                        .map(str::trim)
                        .filter(|label| !label.is_empty())?;
                    let description = item
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let preview = item
                        .get("preview")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|text| !text.trim().is_empty());
                    Some(QuestionOption {
                        label: label.to_string(),
                        description,
                        preview,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let multi_select = object
        .get("multiSelect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((question, header, options, multi_select))
}

fn sender_from(channel: &QuestionChannel) -> Option<UnboundedSender<UserQuestion>> {
    channel
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 发送一个问题并等待回答；取消（Ctrl-C）与超时都返回 `None`。
async fn ask(
    channel: &QuestionChannel,
    abort: &harn_core::abort::AbortSignal,
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    multi_select: bool,
) -> Option<Vec<String>> {
    let sender = sender_from(channel)?;
    let (reply, rx) = oneshot::channel();
    if sender
        .send(UserQuestion {
            question,
            header,
            options,
            multi_select,
            reply,
        })
        .is_err()
    {
        return None;
    }
    tokio::select! {
        biased;
        () = abort.cancelled() => None,
        answer = rx => answer.ok().flatten(),
        () = tokio::time::sleep(ASK_TIMEOUT) => None,
    }
}

pub struct AskUserQuestionTool {
    questions: QuestionChannel,
}

impl AskUserQuestionTool {
    pub fn new(questions: QuestionChannel) -> Self {
        Self { questions }
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &'static str {
        "AskUserQuestion"
    }

    fn description(&self) -> &'static str {
        "Ask the user one or more questions and wait for their answers. Use when a requirement \
         is ambiguous and guessing wrong would be expensive. Each question carries 2-8 options \
         the user can pick from —— the picker also offers a synthesized `Other` item for free-form \
         answers, so never declare an Other/Skip option yourself."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "questions": {
                    "type": "array",
                    "description": "Questions to ask (1-4), asked in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "The full question."},
                            "header": {"type": "string", "description": "Short label (<=12 chars) shown on the picker."},
                            "multiSelect": {"type": "boolean", "description": "Allow picking multiple options."},
                            "options": {
                                "type": "array",
                                "description": "Answer choices (2-8).",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string", "description": "Why this option matters."},
                                        "preview": {"type": "string", "description": "Preview shown when this option is selected (single-select only)."}
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question", "options"]
                    }
                }
            }),
            &["questions"],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn always_allow(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let items = input
            .get("questions")
            .and_then(Value::as_array)
            .ok_or_else(|| HarnError::Tool(harn_i18n::tr!("tool.ask.questions_required")))?;
        if items.is_empty() {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "tool.ask.questions_not_empty"
            )));
        }
        if items.len() > MAX_QUESTIONS {
            return Err(HarnError::Tool(harn_i18n::tr!(
                "tool.ask.too_many_questions",
                n = MAX_QUESTIONS
            )));
        }
        for item in items.iter() {
            let (_, _, options, _) = parse_question(item)?;
            if options.len() < 2 {
                return Err(HarnError::Tool(harn_i18n::tr!("tool.ask.options_minimum")));
            }
            if options.len() > MAX_OPTIONS {
                return Err(HarnError::Tool(harn_i18n::tr!(
                    "tool.ask.too_many_options",
                    n = MAX_OPTIONS
                )));
            }
        }

        if sender_from(&self.questions).is_none() {
            return Ok(ToolOutput::ok(
                "No interactive UI is available to ask the user. Do not ask again this turn: \
                 make your best judgement from the information you already have and continue.",
            ));
        }

        let mut transcript = String::new();
        for (index, item) in items.iter().enumerate() {
            let (question, header, options, multi_select) = parse_question(item)?;
            let chosen = ask(
                &self.questions,
                &ctx.abort,
                question.clone(),
                header,
                options,
                multi_select,
            )
            .await;
            let Some(chosen) = chosen else {
                return Ok(ToolOutput::ok(format!(
                    "The user dismissed the question (or it timed out). \
                     Continue with your best judgement; question was: {question}"
                )));
            };
            if !transcript.is_empty() {
                transcript.push('\n');
            }
            transcript.push_str(&format!(
                "Q{}: {question}\nA{}: {}",
                index + 1,
                index + 1,
                chosen.join(", ")
            ));
        }
        Ok(ToolOutput::ok(format!(
            "User answers:\n{transcript}\n\nProceed accordingly."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question_with_string_and_object_options() {
        let value = json!({
            "question": "Which deploy target?",
            "header": "Deploy",
            "multiSelect": true,
            "options": [
                "staging",
                {"label": "prod", "description": "careful"}
            ]
        });
        let (question, header, options, multi) = parse_question(&value).unwrap();
        assert_eq!(question, "Which deploy target?");
        assert_eq!(header, "Deploy");
        assert!(multi);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "staging");
        assert_eq!(options[1].label, "prod");
        assert_eq!(options[1].description.as_deref(), Some("careful"));
    }

    #[test]
    fn rejects_question_without_options() {
        let value = json!({"question": "huh?"});
        assert!(parse_question(&value).is_ok());
        let (question, _, options, _) = parse_question(&value).unwrap();
        assert_eq!(question, "huh?");
        assert!(options.is_empty());
    }

    #[tokio::test]
    async fn missing_ui_degrades_to_guidance_text() {
        let channel: QuestionChannel = std::sync::Arc::default();
        let tool = AskUserQuestionTool::new(channel);
        let policy = harn_permission::PermissionPolicy::new(
            harn_config::PermissionMode::Auto,
            &harn_config::PermissionsSettings::default(),
            std::sync::Arc::new(harn_permission::NonInteractivePrompter::new(true)),
            std::env::temp_dir(),
            true,
        )
        .unwrap();
        let ctx = ToolContext {
            cwd: std::env::temp_dir(),
            session_id: "s".into(),
            session_dir: std::env::temp_dir(),
            events: harn_core::event::EventSink::noop(),
            agent_id: "main".into(),
            depth: 0,
            permission: std::sync::Arc::new(policy),
            plan: std::sync::Arc::new(crate::plan::PlanState::default()),
            spawner: std::sync::Arc::new(std::sync::OnceLock::new()),
            tasks: std::sync::Arc::new(crate::tasks::TaskList::new()),
            abort: harn_core::abort::AbortSignal::new(),
            lsp: None,
            worktrees: None,
            session_worktree: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        let out = tool
            .call(
                json!({"questions": [{"question": "q?", "options": ["a", "b"]}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No interactive UI"));
    }
}
