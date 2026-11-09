//! harn-agent：主循环、工具并发调度、子代理运行器与注册表。

pub mod agent;
pub mod manager;
pub mod prompt;

pub use agent::{Agent, AgentConfig, RunOutcome, Termination, UserInput};
pub use manager::{RuntimeDeps, SubagentManager};
