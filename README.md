# harn — Rust Coding-Agent Harness

[![CI](https://github.com/xiaocongyu66/harn/actions/workflows/ci.yml/badge.svg)](https://github.com/xiaocongyu66/harn/actions/workflows/ci.yml)

一个用 Rust 实现的 coding-agent harness：交互 REPL、无头模式、ratatui TUI、流式工具调用、
六种权限模式、多模态输入、子代理编排、Plan 模式、命令钩子与会话恢复。

设计参考：

- Claude Code 反编译源码（`/root/xiaocongyu66-claude-code`）
- 《御舆：解码 Agent Harness》`lintsinghua/claude-code-book`
- 《Claude Code 实战：Harness 工程之道》的工程视角（工具、知识、观察、行动、权限）

## 快速开始

```bash
cargo build --release
./target/release/harn                 # REPL（无 API key 时自动使用离线 mock）
./target/release/harn -p "hello"      # 无头模式
./target/release/harn --tui           # ratatui TUI
```

离线 mock 会回显最后一条用户输入；用脚本可以驱动完整的工具调用：

```json
{
  "turns": [
    { "text": "write it", "tool_use": [
      { "name": "Write", "input": { "file_path": "hello.txt", "content": "hi\n" } }
    ]},
    { "text": "done" }
  ]
}
```

```bash
HARN_MOCK_SCRIPT=$PWD/script.json harn -p "create hello.txt" --permission-mode acceptEdits
```

## 接入真实模型

### Anthropic Messages API

```bash
export ANTHROPIC_API_KEY=sk-ant-...
export ANTHROPIC_MODEL=claude-sonnet-4-6       # 可选
export ANTHROPIC_BASE_URL=https://api.anthropic.com  # 可选，兼容网关
harn -p "explain this repo"
```

### OpenAI Chat Completions

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_BASE_URL=https://api.openai.com/v1   # 任意兼容网关
harn --provider openai --openai-api chat -p "hello"
```

### OpenAI Responses API

```bash
harn --provider openai --openai-api responses -p "hello"
```

两者共享内部消息模型，但使用各自的原生线格式：

| 内部概念 | Chat Completions | Responses API |
|---|---|---|
| system | `role=system` | `instructions` |
| tool_use | `assistant.tool_calls` | `function_call` item |
| tool_result | `role=tool` + `tool_call_id` | `function_call_output` |
| 流式文本 | `delta.content` | `response.output_text.delta` |
| 工具参数流 | `delta.tool_calls[].function.arguments` | `response.function_call_arguments.delta` |
| usage | 末尾 chunk `usage` | `response.completed.response.usage` |

也可以完全离线测试 HTTP 路径：`crates/harn-providers/tests/http.rs` 用本地 SSE 服务器
验证了两种协议的请求体、流式解析、工具调用与 usage。

## 多模态

```bash
harn -p "what is in this screenshot?" --image shot.png
```

REPL/TUI 中：

```text
/image screenshot.png
> describe it
```

支持 png/jpeg/gif/webp，单图上限 5MB。Anthropic 使用 `image` base64 块；
OpenAI Chat 使用 `image_url` data URL；Responses 使用 `input_image` + `detail: auto`。

## 命令行

```text
harn [PROMPT] [OPTIONS]
  -p, --print                 无头模式（提示词可来自 stdin）
  -m, --model <MODEL>
      --provider <NAME>       anthropic | openai | mock
      --openai-api <API>      chat | responses
      --base-url <URL>
      --permission-mode <M>   default | acceptEdits | plan | auto | bypassPermissions | dontAsk
      --dangerously-skip-permissions
  -c, --continue              恢复最近会话
      --resume <ID>           恢复指定会话（支持前缀）
      --max-turns <N>
      --max-tokens <N>
  -y, --yes                   自动批准权限询问与计划审批
      --image <PATH>          可重复
      --coordinator           主代理只编排、不直接执行
      --tui
  -v, --verbose

harn sessions list|show <ID>
harn config show|path
harn doctor
harn agents
```

斜杠命令：`/help /status /model /permissions /plan /auto /bypass /default
/accept-edits /dont-ask /cost /clear /compact /image /agents /sessions /exit`。

`/permissions` 管理权限规则（对齐 Claude Code 的 PermissionUpdate）：

```text
/permissions list                      列出各层生效规则（含来源）与遮蔽警告
/permissions allow "Bash(cargo test:*)"  写入项目本地 settings（默认）
/permissions allow "Read(src/**)" --user 写入用户 settings
/permissions deny  "Bash(rm:*)" --policy 之外的层由 --user/--local/--session 选择
/permissions remove allow "Read(src/**)"
/permissions mode auto --local          设置默认模式
```

文件层更新即时重载生效；TUI 权限弹层的「始终允许并写入本地设置」与之同一机制。

## TUI 主题

TUI（`--tui`）的视觉由 [`ratatui-style`](https://crates.io/crates/ratatui-style) 的 CSS
级联引擎驱动：选择器优先级、属性继承、`var()` 令牌与伪状态，最终投影为原生
`ratatui::style::Style` / `ratatui::widgets::Block`，不是并行渲染栈。

默认主题移植 Claude Code 的暗色视觉语言（`crates/harn-cli/src/theme.rs` 的 `DEFAULT_CSS`）：

```css
:root {
  --fg: #ffffff;      --inactive: #999999;  --subtle: #505050;
  --user-bg: #373737; /* 用户消息色块 */
  --bash-bg: #413c41; /* bash 行色块 */
  --claude: #d77757;  /* 品牌橙（流式呼吸色起点） */
  --suggestion: #b1b9f9;   --success: #4eba65;
  --error: #ff6b80;   --warning: #ffc107;   --plan: #48968c;
  --accept: #af87ff;  /* autoAccept 电紫 */
}

Line.you  { color: var(--fg); background: var(--user-bg); font-weight: bold; }
Line.ok   { color: var(--success); }
Line.error{ color: var(--error); font-weight: bold; }

Input { border-style: rounded; border-color: var(--border); }
Input.mode-plan    { border-color: var(--plan); }     /* 输入框描边跟随权限模式 */
Input.mode-accept  { border-color: var(--accept); }
Input.mode-bypass  { border-color: var(--error); }

Mode.accept { color: var(--accept); }                 /* 徽标颜色随模式切换 */
Mode.bypass { color: var(--error); }                  /* 危险模式即红 */
```

视觉要点（全部对齐 Claude Code 源码观察）：

- **用户消息色块**：整行 `#373737` 底 + 粗体白字，从流式文本中跳出（CC `userMessageBackground`）。
- **工具行**：工具名粗体，状态标记 `· ▶ ✓ ✗ ⊘` 用状态色（CC `AssistantToolUseMessage`）。
- **模式徽标**：`⏸ plan` 青 / `⏵⏵ accept` 电紫 / `⏵⏵ auto` 琥珀 / `⏵⏵ bypass` 红，
  危险模式直接红（CC `getModeColor`）。
- **输入框**：`❯` 提示符 + 底部圆角线 `╰──╯`，描边色随模式切换；空闲时按模式显示
  斜体占位提示。
- **流式呼吸**：正在生成的当前行在品牌橙 ↔ 蓝紫之间缓移（250ms 节拍，8 帧三角波），
  侧栏进行中任务同步旋转 braille spinner。
- **状态栏分段着色**：模式徽标亮色、运行态 `● running` 琥珀、任务 activeForm 蓝紫、
  其余段（provider/model/tokens/shell/RSS/pid）降灰；RSS 超 512MB 转琥珀、超 1.5GB 转红。
- 转录区全宽流式无外框，离底时右下角 `↑ N more` 滚动角标。

可用自己的 CSS 覆盖（`HARN_THEME_CSS` 指定文件，或 `<HARN_HOME>/theme.css`）：

```css
/* ~/.harn/theme.css */
:root { --user-bg: #264f78; --success: #3fb950; }
Line.ok { color: #3fb950; }
Mode.bypass { color: #ff4444; }
```

用户 CSS 以 `Origin::User` 层叠加在默认主题之上：同字段按选择器优先级与来源层裁决，
未提及的字段保持默认值。主题损坏不会导致程序退出——只降级到默认主题并在转录区给出
`[error] theme: ...` 提示。

设计要点：

- **继承**：`Line` 从 `Transcript` 继承 `color`，各角色 class 再覆盖，规则只写一次。
- **预计算**：静态样式（输入框描边、状态栏、弹层、六个模式徽标）在加载时一次性 `compute`，
  绘制时零分配返回原生 `Block`/`Style`；只有转录行角色在运行期惰性计算。
- **可测**：`crates/harn-cli/src/tui.rs` 用 ratatui `TestBackend` 断言真实缓冲区内容
  （用户行底色块、工具名加粗、模式色、输入提示符、状态栏文案）而非依赖 PTY；
  `scripts/tui_full_smoke.py` 再在真实 PTY 上验证 25 项交互。

界面能力：

- 转录区：流式文本增量合并、工具生命周期（`·` 开始 / `▶` 运行 / `✓` 成功 /
  `✗` 失败 / `⊘` 拒绝）、并发批次折叠为一行「N 个并行工具」、结果节流裁剪。
- 侧栏（`Tab`）：会话、token 用量、子代理列表、**任务清单进度**（与 Agent 共享同一份
  `TaskList`，`TaskCreate/TaskUpdate` 后即时刷新）。
- 弹层：权限询问、计划审批、**提问选择**（`AskUserQuestion`：单选回车确认，多选
  空格勾选，`Esc` 或最后一项取消；REPL 下降级为编号选择器）。
- 输入区：行内编辑（Ctrl-A/E/B/F/W/U、Home/End）、↑/↓ 历史（草稿保留）、
  `/` 斜杠命令补全弹层。
- 快捷键：F1 帮助（可翻页）、PgUp/PgDn 滚动、**Shift+Tab 循环切换权限模式**
  （default → acceptEdits → plan → auto → bypassPermissions → dontAsk）、
  Ctrl-C 中断回合、Ctrl-D 退出（输入为空时）。转录渲染带缓存，仅在内容变化时重建。
- 状态栏（对齐 Claude Code）：`provider · model · 模式徽标 · 运行态 · activeForm ·
  tok · agents · N 个 shell · RSS · pid`，分段着色、窄终端自动降级保住核心段；
  运行时若任务清单有进行中的 activeForm，会显示在状态栏。

真实终端可人工验证：

```bash
./target/debug/harn --tui -p "read README.md" --permission-mode auto
```


## 权限模型

优先级：`deny > ask > allow > 模式默认`；`PreToolUse` 钩子可在 deny/plan 约束之后
覆盖 ask/allow。

| 模式 | 只读 | 工作区写 | 安全构建/测试 | 普通变更 | 危险命令 | 未匹配 |
|---|---|---|---|---|---|---|
| default | allow | ask | ask | ask | deny | ask |
| acceptEdits | allow | allow | ask | ask | deny | ask |
| plan | allow | deny | deny | deny | deny | deny（Plan 工具除外） |
| auto | allow | allow | allow | ask | deny | ask |
| bypassPermissions | allow | allow | allow | allow | allow（显式 deny 除外） | allow |
| dontAsk | allow | deny | deny | deny | deny | deny |

规则示例（`~/.harn/settings.json` 或 `<project>/.harn/settings.json`）：

```json
{
  "permissions": {
    "defaultMode": "auto",
    "allow": ["Read", "Bash(git status:*)", "Edit(src/**)"],
    "ask": ["Bash(cargo publish:*)"],
    "deny": ["Bash(rm -rf:*)", "Bash(sudo:*)"]
  }
}
```

- `Bash(git status)`：精确匹配。
- `Bash(cargo test:*)`：前缀匹配。
- `Read(src/**)` / `Edit(src/**)`：路径 glob。
- 规则内容里的括号需转义：`Bash(python -c "print\(1\)")`。
- **复合命令按子命令匹配**：`git status && rm -rf build` 会拆成 `git status` 与
  `rm -rf build` 两个子命令——allow 规则必须覆盖**每一个**子命令才放行，任一子命令
  命中 deny 规则即拒绝（堵住单条前缀规则顺带放行危险命令的缺口）。
- 子代理继承父级模式与规则，只能通过工具白名单收窄，不能提权。

**规则来源分层**（与 Claude Code 的 `PermissionRuleSource` 对齐）。每条规则按来源参与
匹配与遮蔽检测，优先级从高到低：

| 层 | 文件 |
|---|---|
| `policySettings` | `/etc/harn/managed-settings.json`（或 `$HARN_POLICY_FILE` 指定），企业管控、只读 |
| `userSettings` | `~/.harn/settings.json` |
| `projectSettings` | `<project>/.harn/settings.json` |
| `localSettings` | `<project>/.harn/settings.local.json` |

- policy 层的 deny 压过所有低层 allow；会话内工具（`AllowAlways`）只在 `session` 层记忆。
- 加载期检测**规则遮蔽**（高层宽规则让低层窄规则永不生效），以
  `WARN: 权限规则 ... 被 ... 遮蔽` 打印（`harn doctor` 与 TUI 启动提示均会显示）。
- 每次放行/拒绝都带**结构化决策理由**（命中规则[来源] / 模式默认 / 钩子干预 /
  危险命令 / plan 限制 / 用户选择），拒绝时直接展示给模型与用户，例如
  `⊘ Bash git status && rm -rf build denied — 命中 deny 规则 Bash(rm:*) [projectSettings]`。

## 工具

| 工具 | 说明 |
|---|---|
| `Bash` | `bash -c`，默认 120s 超时，进程组 SIGKILL，ANSI 清理，>30k 字符落盘 |
| `Read` | 行号读取，二进制拒绝，offset/limit |
| `Write` | 创建/覆盖，自动建父目录 |
| `Edit` | 精确替换；重复匹配需 `replace_all` |
| `Glob` | 移植的 bfs 语义文件发现（mtime 排序） |
| `Grep` | 移植的进程内 ripgrep（content/files/count、上下文、glob/type） |
| `Agent` | 子代理（general/explore/plan/verify，可后台；`isolation=worktree` 让 general 子代理在独立 git worktree 里干活，改动统计与合并命令随结果返回） |
| `AgentList` / `AgentStop` | 主代理查看/取消子代理 |
| `Team` | 并行运行多个子代理（默认并发 3，上限 8） |
| `TaskCreate` / `TaskUpdate` / `TaskList` | 会话级任务清单（○ pending / ▶ in_progress / ✓ completed，上限 64） |
| `TodoWrite` | 整体替换式清单（对齐 Claude Code TodoWrite：content/status/activeForm/blockedBy 一次写入） |
| `AskUserQuestion` | 通过 TUI 弹层或 REPL 选择器向用户提问；无交互界面时明确告知模型自行决策 |
| `WebFetch` | 抓取 http(s) 页面转纯文本（SSRF 防护：拒绝内网/环回；仅文本类内容） |
| `NotebookEdit` | 编辑 `.ipynb` cell（replace/insert/delete，按 id 或索引） |
| `lsp` | 语言服务器查询：goToDefinition / findReferences / hover / documentSymbol / workspaceSymbol / goToImplementation / prepareCallHierarchy / incomingCalls / outgoingCalls |
| `EnterWorktree` / `ExitWorktree` / `MergeWorktree` | 会话级 git 工作区：`项目/.harn/worktrees/<slug>` 隔离副本；退出给 keep/remove 与合并命令（删除有改动时 fail-closed）；MergeWorktree 把提交 cherry-pick/merge 进主工作树当前分支（dry-run、冲突回退、主树脏拒绝） |
| `EnterPlanMode` / `ExitPlanMode` | 计划模式与审批 |

工具执行使用**并发分区**：连续的并发安全工具成批并行，非安全工具严格串行，
结果顺序与请求顺序一致。Bash 超时会终止整个进程组，避免孤儿进程。

`Edit` / `Write` 成功后附带 **bat 风格 ANSI 着色 diff**（`harn-tools::diff`，移植自 Claude Code
`color-diff-napi` 的纯算法部分）：jsdiff 兼容的行/词 Myers diff + `structured_patch` hunks，
删除行红底、新增行绿底、词级高亮、行号对齐。

性能设计：Agent 在追加消息时维持「连续同角色已合并」的归一化不变量，消息序列以
`Arc<Vec<Message>>` 在各回合间零拷贝共享（不再每轮深拷贝）；provider 只做线格式映射。
工作区 clippy 启用 `unwrap_used` / `expect_used`（测试内豁免），生产路径的错误一律沿
`Result` 传播，长跑会话不会因单个假设落空而 panic。

`Grep`/`Glob` 直接复用 Claude Code 仓库中的 `packages/search-napi/native`
内核（去除 N-API 绑定），保留 grep-regex/grep-searcher/PCRE2 降级链、bfs 遍历、
隐藏文件与 gitignore 语义。

### 对话渲染（Markdown）

TUI 的对话输出是 Markdown 渲染的（移植 Claude Code `src/utils/markdown.ts` 的
终端呈现）：标题（粗体/下划线）、粗体/斜体/行内码（蓝）、列表（`•`，有序按深度
数字→字母→罗马）、引用（`▎` 前缀斜体）、代码块（syntect 语法高亮 + 语言标签，
六类映射到主题色）、表格（box-drawing 边框，超宽降 key-value 竖排）、分割线；
assistant 首行 `⏺`、user `❯` + 灰底色块。行内样式与高亮经
`harn_ui::markdown`/`highlight` 投影为 `RenderLine{tag, spans}`，颜色由主题 CSS
单一来源决定。流式期间每个 delta 后全量重解析（未闭合 fence 自然渲染到文尾）。
diff（Edit/Write）输出标记化文本（行首 `+`/`-` + `[n]` gutter 行号），TUI 上
diff-add/del 行级底色与 gutter——不再有 ANSI 乱码。

### 工作区（子代理隔离）

主代理派发子代理有三种工作方式：默认共享工作树；`Agent` 加 `isolation:"worktree"`
（可配 `worktreeName`）则在**项目内** `项目/.harn/worktrees/<slug>` 建一个 git worktree
（分支 `worktree-<slug>`，默认基于当前 HEAD），子代理的全部读写只落在该副本里，
主工作树不受影响。只读子代理拒绝该参数（无写动作，建了纯浪费）。子代理结束时：
零改动 → 工作区自动删除；有改动 → 保留，并把改动计数与 git 合并命令
（`git -C <项目> cherry-pick HEAD..worktree-<slug>` 或 `merge`）随结果返回主代理——
合并与否由你/主代理执行，harn 不自动改仓库。

主代理自己也能切换：`EnterWorktree [name]` 进入、`ExitWorktree {action:"keep"|"remove"}`
退出（remove 遇未提交/未合并改动会拒绝，需显式 `discardChanges:true`）。会话结束时
汇总所有存活工作区。非 git 仓库会得到明确报错（建议 `git init`），不静默降级。

**合并回主项目**（`MergeWorktree`）：`mode=cherry-pick`（默认）把工作区的提交线性
重放进主工作树当前分支；`mode=merge` 生成一个 `--no-ff` 合并提交。`dryRun=true`
先列出将进入的提交与前置检查（不碰任何状态）。冲突时自动 `--abort` 回退，
绝不在主树留下半合并状态；主工作树有未提交改动时拒绝合并（保护在途工作）。
注意合并只带入**已提交**内容——worktree 里未提交的改动仍留在原地，先 commit
或 `ExitWorktree keep` 查看。`.harn/worktrees/` 已写入主仓 `info/exclude`，
用户的 `git add .` 不会把工作区当嵌入式仓库收进去。
settings 可选配置（默认 HEAD 基线、全量检出）：

```json
{"worktree": {"baseBranch": "main", "symlinkDirectories": ["node_modules"], "sparsePaths": ["src"]}}
```

### 语言服务器（LSP）

`lsp` 工具把外部语言服务器接进 Agent 循环（移植 Claude Code `src/services/lsp` 的
交互模型：同样的九个操作与 1-based 行列入参）。服务器从 settings 的 `lspServers` 段
配置——harn 只 spawn 用户自己安装的命令，不代装、不代管：

```json
{"lspServers": {"rust-analyzer": {
  "command": "rust-analyzer", "args": [], "filetypes": ["rs"], "rootMarker": "Cargo.toml"
}}}
```

`rootMarker` 向上寻找工作区根（`.` 表示当前目录），`filetypes` 决定路由。
相对 Claude Code 的三处更新：`lsp-types` 0.97（LSP 3.17）替代其 vscode-jsonrpc 绑定；
**位置编码协商**（服务器声明 utf-8 时按字符列，否则回退 LSP 默认的 UTF-16；
CC 缺失此项，多字节字符旁跳转会偏移）；配置经 settings 六层分层（不限于插件通道）。
工具自动补 `didOpen`（CC 需要模型先读文件），诊断经 `publishDiagnostics` 收集，
编辑后如有 error 级问题会随工具结果反馈。

## 子代理与多 Agent

- `general`：可读写；可再派生子代理（深度上限默认 2）。
- `explore`：只读探索。
- `plan`：只读规划。
- `verify`：对抗性验证。
- 子代理拥有独立上下文与 Provider 实例，结束后只向父代理返回摘要。
- 主代理通过 `AgentList` 维护子代理列表，通过 `AgentStop` 取消后台任务。
- `Team` 使用 `buffer_unordered` 并行执行，聚合结果。
- `--coordinator`：主代理只保留 `Agent / AgentList / AgentStop / Team / ExitPlanMode`，
  不直接读写，把所有执行交给 workers。

## Plan 模式

```bash
harn --permission-mode plan -p "design the pagination feature"
```

- 只读工具可用；写入与变更命令被拒绝。
- 代理调用 `ExitPlanMode` 提交计划。
- 计划写入会话目录下的 `plans/plan-<uuid>.md`。
- 批准后恢复进入 Plan 前的模式（默认 `acceptEdits`）；拒绝则继续规划。
- 子代理禁止进入 Plan 模式。

## 钩子

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "bash ./check.sh" } ] }
    ],
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "bash ./inject-context.sh" } ] }
    ]
  }
}
```

事件：`PreToolUse`、`PostToolUse`、`PostToolUseFailure`、`UserPromptSubmit`、
`SessionStart`、`SessionEnd`、`Stop`。

- stdin：JSON（event、cwd、session_id、agent_id、tool_name、tool_input、prompt）。
- 退出码 2：阻断，stderr 作为原因。
- stdout JSON：`{"decision":"block","reason":"..."}` 或
  `{"hookSpecificOutput":{"permissionDecision":"allow|deny|ask","additionalContext":"..."}}`。
- 默认超时 60s，可配置 `timeout`。

## 会话

- JSONL 转录：`~/.harn/projects/<cwd-hash>/<session-id>.jsonl`（可用 `HARN_HOME` 覆盖）。
- 记录 meta、消息、usage 与备注。
- `--continue` 恢复最近会话，`--resume <id>` 指定会话。
- `harn sessions list` / `harn sessions show <id>` 查看。
- 工具大输出保存在 `<session-dir>/tool-results/`。
- 子代理状态落盘在 `<session-dir>/agents/<id>/status.json`；TUI 启动时回填展示，
  上次会话未收尾的 `running` 记录按中断处理（标为 cancelled 并注明「上次会话中断」）。

## 架构（Cargo workspace）

```
harn-core        消息/事件/取消/Provider 契约/子代理契约
harn-config      分层 settings、Provider/模型/路径解析
harn-search      search-napi 移植内核（grep/find/bfs/sanitize/heart）
harn-permission  规则、Bash 风险分类、权限策略、交互提示
harn-providers   Anthropic / OpenAI Chat / OpenAI Responses / Mock
harn-tools       Tool trait、注册表、内置工具（含 diff 内核：jsdiff Myers + bat ANSI 着色、LSP 客户端）
harn-hooks       命令钩子
harn-session     JSONL 转录
harn-agent       主循环、并发分区、子代理管理器、系统提示
harn-ui          纯文本流式渲染
harn-cli         clap、REPL、TUI、子命令、二进制
```

依赖方向单向：`harn-cli → harn-agent → {tools, permission, providers, ...}`，
共享类型下沉到 `harn-core`，不存在循环依赖。

## 安全边界

- 路径规则默认限定工作区；越界触发 ask/deny。
- 危险命令（`rm -rf /`、`mkfs`、`dd of=/dev/*`、管道执行远程脚本、force push 等）
  在非 bypass 模式下拒绝。
- `bypassPermissions` 只跳过交互确认，显式 deny 规则仍然生效。
- 子代理深度上限默认 2，只读类型在注册表中物理移除写工具。
- 钩子有超时与结构校验；非 JSON stdout 不参与决策。
- API key 只从环境变量读取，不写入转录。
## 开发与验证

本地三件套与 CI 完全一致（CI 即 GitHub Actions 两 job 并行：lint=fmt+clippy
`-D warnings`，test=`cargo test --workspace`，均带 swatinem/rust-cache 缓存）：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**本机推送**：Android/Termux 沙箱对 github.com 的 git 传输通道几乎不可用
（上传速度 <1KB/s，`git push` 必然超时），但 `api.github.com` 正常。推送方式：
`gh repo create --public --source=.` 建仓后，用 GitHub Git Data API 逐对象上传
（`git ls-files` → blob → 目录 tree → commit → `PATCH git/refs/heads/main`）——
脚本要点见 `scripts/` 说明或 `PLAN.md`；增量推送同样只传改动的 blob。

测试覆盖：

- 移植搜索内核 71 项（grep/find/bfs）。
- Provider SSE 解析 + 本地 HTTP 端到端（Chat / Responses）。
- 权限矩阵、Bash 风险分类、规则匹配、路径归一化。
- CLI 集成 17 项：无头模式、工具循环、plan 拒绝/批准、子代理/Team/后台列表、
  会话恢复、钩子阻断与 allow 覆盖、Bash 超时/大输出落盘、危险命令拒绝。
- 子代理后台列表与取消单元测试。


## 许可证

本项目采用 [Blue Oak Model License 1.0.0](LICENSE)（宽松许可证：允许使用/修改/分发，
无 copyleft；要求保留许可声明；无专利/商标担保之外的明示担保）。英文原文见 `LICENSE`。
