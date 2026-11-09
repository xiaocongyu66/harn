//! harn-core：跨 crate 的领域类型、事件总线、取消信号与 Provider 契约。

pub mod abort;
pub mod error;
pub mod event;
pub mod image;
pub mod message;
pub mod provider;
pub mod subagent;

pub use abort::AbortSignal;
pub use error::{HarnError, Result};
pub use event::{AgentEvent, EventSink};
pub use image::{MAX_IMAGE_BYTES, image_block_from_path};
pub use message::{AssistantTurn, ContentBlock, Message, Role, ToolDef, Usage, normalize_messages};
pub use provider::{Provider, ProviderFactory, ProviderRequest, ProviderRole};
pub use subagent::{SubagentInfo, SubagentKind, SubagentOutput, SubagentSpawner};
