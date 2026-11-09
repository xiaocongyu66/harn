//! harn-providers：Anthropic / OpenAI Chat / OpenAI Responses / Mock。

pub mod anthropic;
pub mod mock;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use mock::{MockProvider, MockScript};
pub use openai::OpenAiProvider;

pub use harn_config::OpenAiApi;

/// 统一 User-Agent：所有出站 API 请求携带，供网关侧识别客户端。
/// 形态对齐 Claude Code 的 `claude-cli/<ver> (<type>, <entrypoint>)`。
pub const USER_AGENT: &str = concat!("harn-cli/", env!("CARGO_PKG_VERSION"), " (external, cli)");
