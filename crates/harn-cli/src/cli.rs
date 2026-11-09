//! clap 命令行定义。

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harn_config::{ConfigOverrides, OpenAiApi, PermissionMode};

#[derive(Debug, Parser)]
#[command(
    name = "harn",
    version,
    about = "harn — a Rust coding-agent harness",
    long_about = harn_i18n::tr!("cli.about.long")
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// 无头模式：执行一次提示后退出（可与 stdin 管道配合）。
    #[arg(short = 'p', long = "print", global = true)]
    pub print: bool,

    /// 初始提示词；交互模式下作为首轮输入。
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,

    /// 覆盖模型名。
    #[arg(short = 'm', long, global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Provider：anthropic / openai / mock，或 openai-chat / openai-responses。
    #[arg(long, global = true, value_name = "NAME")]
    pub provider: Option<String>,

    /// OpenAI 协议：chat 或 responses。
    #[arg(long = "openai-api", global = true, value_name = "API")]
    pub openai_api: Option<OpenAiApi>,

    /// 覆盖 API base URL（当前 provider）。
    #[arg(long = "base-url", global = true, value_name = "URL")]
    pub base_url: Option<String>,

    /// 权限模式。
    #[arg(
        long = "permission-mode",
        global = true,
        value_name = "MODE",
        help = "default | acceptEdits | plan | auto | bypassPermissions | dontAsk"
    )]
    pub permission_mode: Option<PermissionMode>,

    /// 等价于 --permission-mode bypassPermissions。
    #[arg(long, global = true)]
    pub dangerously_skip_permissions: bool,

    /// 恢复当前工作目录最近一次会话。
    #[arg(short = 'c', long = "continue", global = true)]
    pub continue_latest: bool,

    /// 恢复指定会话 id（支持前缀）。
    #[arg(long, global = true, value_name = "ID")]
    pub resume: Option<String>,

    /// 单次运行的最大 agent 轮数。
    #[arg(long, global = true, value_name = "N")]
    pub max_turns: Option<usize>,

    /// 单次响应最大输出 token。
    #[arg(long, global = true, value_name = "N")]
    pub max_tokens: Option<u32>,

    /// 自动批准权限询问与计划审批（deny 规则仍然生效）。
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// 追加系统提示。
    #[arg(long, global = true, value_name = "TEXT")]
    pub system_prompt: Option<String>,

    /// 图片输入（可重复）；与首轮提示一起发送。
    #[arg(long, global = true, value_name = "PATH")]
    pub image: Vec<PathBuf>,

    /// Coordinator 模式：主代理只编排，不直接读写。
    #[arg(long, global = true)]
    pub coordinator: bool,

    /// 使用 ratatui 全屏 TUI。
    #[arg(long, global = true)]
    pub tui: bool,

    /// Web UI 模式：本地起服务，浏览器操控（默认 127.0.0.1:7331）。
    #[arg(long, global = true)]
    pub web: bool,

    /// Web UI 监听端口。
    #[arg(
        long = "web-port",
        global = true,
        value_name = "PORT",
        default_value = "7331"
    )]
    pub web_port: u16,

    /// Web UI 绑定地址（默认 127.0.0.1；局域网访问传 0.0.0.0）。
    #[arg(
        long = "web-host",
        global = true,
        value_name = "HOST",
        default_value = "127.0.0.1"
    )]
    pub web_host: String,

    /// 输出调试信息到 stderr。
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 查看与恢复会话。
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// 查看配置。
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 环境与 provider 自检。
    Doctor,
    /// 列出内置子代理类型。
    Agents,
    /// 启动/管理常驻守护进程（Runtime 持久化，TUI/Web/CLI 共享）。
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServerAction {
    /// 启动守护进程（UDS 监听）。同 socket 已有存活进程则直接返回。
    Start {
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
        /// 守护进程显示名（默认随机形容词）。
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// exec 热重载继承的监听 fd（内部使用：跳过 bind，直接接管）。
        #[arg(long, value_name = "FD", hide = true)]
        socket_fd: Option<i32>,
        /// 同时在 loopback 暴露 Web UI（端口见 --web-port）。
        #[arg(long)]
        web: bool,
        /// 关闭空闲看门狗（默认无连接 5 分钟退出）。
        #[arg(long)]
        keep_alive: bool,
    },
    /// 停止运行中的守护进程。
    Stop {
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
    },
    /// 查看守护进程状态（注册表内容 + 可达性）。
    Status {
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
    },
    /// 热重载：exec 进新二进制，同 socket，客户端自动重连。
    Reload {
        #[arg(long, value_name = "PATH")]
        socket: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionsAction {
    /// 列出当前工作目录的会话。
    List,
    /// 输出指定会话的转录。
    Show { id: String },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// 输出合并后的配置。
    Show,
    /// 输出配置目录路径。
    Path,
}

impl Cli {
    pub fn overrides(&self) -> ConfigOverrides {
        let permission_mode = if self.dangerously_skip_permissions {
            Some(PermissionMode::BypassPermissions)
        } else {
            self.permission_mode
        };
        ConfigOverrides {
            model: self.model.clone(),
            provider: self.provider.clone(),
            openai_api: self.openai_api,
            base_url: self.base_url.clone(),
            max_tokens: self.max_tokens,
            max_turns: self.max_turns,
            permission_mode,
            non_interactive: self.print,
            auto_approve: self.yes,
            coordinator: self.coordinator,
            session_id: self.resume.clone(),
            continue_latest: self.continue_latest,
            system_prompt: self.system_prompt.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_headless_prompt() {
        let cli = Cli::try_parse_from([
            "harn",
            "-p",
            "hi",
            "--permission-mode",
            "acceptEdits",
            "--image",
            "a.png",
        ])
        .unwrap();
        assert!(cli.print);
        assert_eq!(cli.prompt.as_deref(), Some("hi"));
        assert_eq!(cli.permission_mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(cli.image.len(), 1);
    }

    #[test]
    fn parses_openai_modes() {
        let cli =
            Cli::try_parse_from(["harn", "--provider", "openai", "--openai-api", "responses"])
                .unwrap();
        assert_eq!(cli.openai_api, Some(OpenAiApi::Responses));
    }
}
