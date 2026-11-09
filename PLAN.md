# harn — 严格实施计划（Rust Coding-Agent Harness）

> 参考：
> - `/root/xiaocongyu66-claude-code`（Claude Code 反编译源码）
> - `/root/hjimicode/reference/claude-code-book`（《御舆：解码 Agent Harness》）
>
> 设计对齐书中六大核心：对话循环、工具系统、权限管线、子代理、多智能体编排、Plan 模式。
> 本计划是硬性验收清单：每个里程碑必须通过 `cargo fmt --check`、`cargo clippy -D warnings`、
> `cargo test` 对应测试后才可标记完成。

## 0. 硬性约束

1. **Cargo workspace 多 crate**，禁止单 crate 堆叠。
2. 依赖方向单向，禁止循环依赖（通过 trait/类型下沉到 `harn-core` 解决）。
3. 所有外部输入（模型输出、工具参数、配置、钩子输出）必须验证。
4. 权限默认 fail-closed；子代理只能收窄权限，不能提权。
5. 每个里程碑结束运行验证门禁；失败不得进入下一里程碑。

## 1. 目标与非目标

### 目标
- 单一二进制 `harn`：交互 REPL、`-p` 无头、TUI、会话恢复。
- Provider：Anthropic Messages、OpenAI Chat Completions、OpenAI Responses、离线 Mock。
- 多模态：`--image` / `/image` 图片输入，Anthropic 与 OpenAI 视觉格式转换。
- 工具：Bash / Read / Write / Edit / Glob / Grep / Agent / Team / AgentList / AgentStop /
  EnterPlanMode / ExitPlanMode。
- 权限：default / acceptEdits / plan / auto / bypassPermissions / dontAsk（+ 内部 bubble）。
- 子代理：general / explore / plan / verify；前台/后台；主代理维护注册表与生命周期。
- 钩子：PreToolUse / PostToolUse / PostToolUseFailure / UserPromptSubmit / SessionStart /
  SessionEnd / Stop。
- 会话：JSONL 转录、`--continue` / `--resume`、计划文件持久化。

### 非目标
- MCP、插件市场、远程控制、IDE 桥接、语音、学习模式。
- 与 Anthropic/OpenAI 之外的专有协议对接。
- 完整键位级 TUI 复刻。

## 2. Workspace 与依赖方向

```
crates/
  harn-core        消息/内容块/事件/取消/Provider trait/子代理契约/错误
  harn-config      分层 settings、ProviderKind、OpenAI/Anthropic 配置、路径
  harn-search      移植的 search-napi 内核（grep/find/bfs/sanitize/heart）
  harn-permission  规则、Bash 分类、权限策略、交互提示
  harn-providers   anthropic / openai(chat+responses) / mock / SSE 解析
  harn-tools       Tool trait、注册表、全部内置工具
  harn-hooks       命令钩子执行器
  harn-session     JSONL 转录、恢复、计划文件
  harn-agent       Agent 主循环、工具并发分区、子代理运行器/注册表、系统提示
  harn-ui          plain 渲染、ratatui TUI、权限提示桥
  harn-cli         clap、装配、REPL、无头模式、子命令、二进制 main
tests/             顶层集成测试（assert_cmd + mock provider）
```

依赖 DAG（箭头表示“依赖”）：

```
harn-cli → harn-agent → {harn-tools, harn-permission, harn-providers, harn-hooks, harn-session, harn-config}
harn-tools → {harn-core, harn-permission, harn-search, harn-config}
harn-providers → harn-core (+ reqwest/serde)
harn-permission → {harn-core, harn-config}
harn-hooks → {harn-core, harn-config}
harn-session → harn-core
harn-ui → {harn-core, harn-permission}
harn-config → harn-core
harn-search → 无内部依赖
```

## 3. 里程碑与验收

### M0 重构为 workspace（当前）
- 建立 11 个 crate 与 `workspace.dependencies`。
- 迁移现有 search 内核、core 类型、config、permission、tools 骨架。
- 验收：`cargo check --workspace` 通过；`cargo test -p harn-search` 通过。

### M1 Core + Config + Provider
- `harn-core`：Message/ContentBlock（含 Image）/Usage/AssistantTurn、AgentEvent、AbortSignal、
  Provider trait、ProviderFactory、SubagentKind/Spawner/Info。
- `harn-config`：六层 settings（user→project→local→env→CLI）、ProviderKind、OpenAiApi、
  多 provider 的 key/base_url/model 解析。
- `harn-providers`：
  - Anthropic SSE（text/tool_use/usage）。
  - OpenAI Chat Completions SSE（delta.content、delta.tool_calls、finish_reason、usage）。
  - OpenAI Responses SSE（output_text.delta、function_call_arguments.delta、response.completed）。
  - Mock（脚本回放，支持子代理脚本）。
- 验收：SSE 单测覆盖三种协议；`cargo test -p harn-core -p harn-config -p harn-providers`。

### M2 工具系统 + 搜索内核
- Tool 五要素协议：name/description/schema/权限属性/执行。
- Bash：超时、进程组终止、ANSI 清理、30k 字符截断落盘。
- Read/Write/Edit：行号读取、二进制拒绝、唯一匹配校验。
- Glob/Grep：接 `harn-search`（rg 管线 + bfs find）。
- 并发分区：连续并发安全工具成批并行，非安全工具串行，结果顺序不变。
- 验收：工具单测 + Bash 超时/截断 + Grep/Glob 真实临时目录测试。

### M3 权限管线
- 四阶段：输入校验 → deny/ask/allow 规则 → 工具上下文评估 → 交互/自动决策。
- 模式：default / acceptEdits / plan / auto / bypassPermissions / dontAsk。
- Bash 规则：`Bash(git status)` 精确、`Bash(cargo test:*)` 前缀、路径 glob。
- auto：只读放行、工作区写入放行、构建测试放行、危险命令拒绝、其余询问。
- bubble：子代理权限冒泡给父级提示器（共享 prompter，不改变模式）。
- 验收：模式 × 工具决策矩阵测试；危险命令在非 bypass 下拒绝；子代理不能提权。

### M4 Agent 主循环
- 状态机：`Continue{reason}` / `Terminal{reason}`；终止原因：
  completed / aborted_streaming / aborted_tools / max_turns / model_error /
  prompt_too_long / hook_stopped / plan_rejected。
- Turn 生命周期：预处理 → Provider 流式 → 工具分区执行 → 结果回填 → 下一轮。
- 依赖注入：ProviderFactory、Spawner、SessionWriter、EventSink 均可替换。
- Ctrl-C 取消：流式请求与子进程同时响应。
- 验收：mock 驱动多轮工具调用；中断不破坏转录一致性；termination 原因可断言。

### M5 Hooks + Session
- 钩子：stdin JSON、stdout JSON、退出码 2 阻断、超时、matcher。
- PreToolUse 可 allow/deny/ask 覆盖；PostToolUse 可附加 context。
- Session：meta/message/usage 记录、`--continue`、`--resume`、列表/展示。
- Plan 文件：`~/.harn/plans/<slug>.md`，启动时恢复。
- 验收：钩子阻断/放行测试；恢复后消息与 usage 一致。

### M6 多 Agent
- `harn-agent::SubagentManager`：注册表（id/kind/description/status/summary/abort/时间）。
- `Agent` 工具：前台等待；`run_in_background` 立即返回 id。
- `AgentList` / `AgentStop`：主代理可查询与终止子代理。
- 子代理类型工具白名单 + 只读硬约束 + 深度上限（默认 2）。
- `Team`：并发运行多个子代理（默认并发 3，上限 8），聚合结果。
- Coordinator 模式（`--coordinator`）：主代理只保留 Agent/AgentList/AgentStop/Team/
  ExitPlanMode，不直接执行读写工具。
- 验收：父子代理隔离、后台列表、停止传播、Team 聚合。

### M7 Plan / Auto / Modes
- EnterPlanMode：保存 `pre_plan_mode`，禁止子代理进入。
- ExitPlanMode：计划写文件 + 审批；批准后恢复 `pre_plan_mode`（auto 断路器回退 default）。
- plan 模式硬拒绝写入与变更命令。
- 验收：plan 无法写文件；批准后模式恢复；拒绝后保持只读。

### M8 UI / CLI
- REPL：流式输出、工具进度、权限提示、斜杠命令
  （/help /clear /model /provider /plan /auto /bypass /permissions /agents /cost /resume /exit）。
- `-p`：stdin/参数提示；非交互 ask 默认拒绝，`--yes` 自动批准。
- TUI：转录区 + 输入区 + 状态栏 + 权限弹层。
- 子命令：sessions list/show、config show/path、doctor、agents。
- 验收：`assert_cmd` 集成测试；mock 端到端写文件 + 子代理。

### M9 最终验证
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace`。
- README：安装、配置、Provider（Anthropic/OpenAI Chat/Responses/Mock）、多模态、
  权限矩阵、钩子、子代理、Plan 模式、安全边界。
- 端到端脚本演示。

## 4. 权限矩阵

| 模式 | 只读 | 工作区写 | 安全构建/测试 | 普通变更 | 危险命令 | 未匹配 |
|---|---|---|---|---|---|---|
| default | allow | ask | ask | ask | deny | ask |
| acceptEdits | allow | allow | ask | ask | deny | ask |
| plan | allow | deny | deny | deny | deny | deny（Plan 工具除外） |
| auto | allow | allow | allow | ask | deny | ask |
| bypassPermissions | allow | allow | allow | allow | allow（显式 deny 除外） | allow |
| dontAsk | allow | deny | deny | deny | deny | deny |

优先级：`deny > ask > allow > 模式默认`。子代理：模式与规则继承父级，工具白名单只收窄。

## 5. OpenAI 双协议映射

| 内部概念 | Chat Completions | Responses API |
|---|---|---|
| system | role=system | `instructions` |
| user 文本+图片 | content parts | `input[].content: input_text/input_image` |
| assistant 文本 | role=assistant | role=assistant + output_text |
| tool_use | assistant.tool_calls | `function_call` item |
| tool_result | role=tool + tool_call_id | `function_call_output` item |
| tools | `{type:function,function:{...}}` | `{type:function,name,parameters}` |
| 流式文本 | `delta.content` | `response.output_text.delta` |
| 流式工具参数 | `delta.tool_calls[].function.arguments` | `response.function_call_arguments.delta` |
| usage | 末尾 chunk.usage | `response.completed.response.usage` |

配置：`HARN_PROVIDER=openai`, `HARN_OPENAI_API=chat|responses`,
`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `HARN_MODEL`/`OPENAI_MODEL`。

## 6. 多模态

- CLI `--image <path>`（可重复）、REPL `/image <path>`。
- 支持 png/jpeg/gif/webp；读取后 base64，限制 5MB。
- Anthropic：`{type:image, source:{type:base64,media_type,data}}`。
- OpenAI Chat：`{type:image_url,image_url:{url:data:...}}`。
- OpenAI Responses：`{type:input_image,image_url:data:...}`。
- Mock 忽略图片但在文本中标注数量。

## 7. 验收命令（每阶段）

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
HARN_HOME=$(mktemp -d) HARN_PROVIDER=mock ./target/debug/harn -p "hello"
```

## 8. 安全边界

- 工具路径默认限定工作区；越界触发 ask/deny。
- Bash 默认 120s 超时、进程组 SIGKILL、30k 字符截断落盘。
- 子代理深度默认 2，只读类型物理移除写工具。
- bypassPermissions 跳过提示但显式 deny 仍生效。
- 钩子超时、退出码与 JSON 结构全部校验。
- 配置/密钥只从环境或 settings 读取，不回显完整密钥。

---

## 9. 实施状态（2026-11-06）

### 已完成
- [x] M0 workspace 拆分（11 个 crate）、search-napi 去 N-API 移植。
- [x] M1 核心类型、分层配置、Anthropic / OpenAI Chat / OpenAI Responses / Mock Provider。
- [x] M1.5 多模态：`--image`、`/image`、Anthropic/OpenAI 图片线格式。
- [x] M1.6 Responses API：流式文本、function calling、usage、本地 HTTP 端到端测试。
- [x] M2 工具：Bash/Read/Write/Edit/Glob/Grep + 并发分区。
- [x] M2.5 子代理：Agent/Team/AgentList/AgentStop，深度限制与工具收窄。
- [x] M2.6 权限：六模式 + 规则 + hook 覆盖 + 危险命令拦截。
- [x] M2.7 子代理注册表：前后台、状态、取消、单元测试。
- [x] M3 Agent 主循环：Continue/Terminal 状态机、Ctrl-C 取消、会话写入。
- [x] M4 REPL + 无头 + ratatui TUI（转录/输入/状态栏/权限与计划弹层）。
- [x] M5 钩子与 JSONL 会话、`--continue` / `--resume`、计划文件。
- [x] M6 Coordinator 模式（只编排不执行）。
- [x] M7 Plan/Auto/Bypass/Default 等模式与计划审批。
- [x] M8 `sessions / config / doctor / agents` 子命令。
- [x] M9 文档与验证：fmt、clippy `-D warnings`、workspace 测试全绿。

### 验证结果
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `cargo test --workspace`：全部通过；其中 search 内核 71 项、CLI 集成 17 项、
  Provider HTTP 2 项、子代理生命周期 1 项。

### 实现说明与已知限制
- TUI 位于 `harn-cli/src/tui.rs`（需要 `Runtime` 装配），`harn-ui` 提供纯文本渲染；
  依赖方向仍无环。
- 计划文档原写 TUI 在 `harn-ui/tui.rs`，实现时因 `Runtime` 位于 CLI 而调整；
  这是有意的依赖倒置取舍。
- 尚未实现：Fork 模式的字节级上下文继承、自动上下文压缩（仅 `/compact`）、
  MCP、插件、远程控制、完整键位级 TUI 操作。

---

## 10. ratatui-style 主题化（2026-11-07）

### 已完成
- [x] 新增 `ratatui-style = "0.2.0"`（CSS 级联引擎，依赖 ratatui 0.30，输出原生 Style/Block）。
- [x] 新增 `crates/harn-cli/src/theme.rs`：内置 `DEFAULT_CSS` + 用户覆盖
      （`HARN_THEME_CSS` 文件或 `<HARN_HOME>/theme.css`），损坏时降级默认主题只在转录区提示。
- [x] TUI 四个渲染区全部改为级联引擎投影：转录行（按 `[you]/[tool]/[ok]…` 取 class）、
      输入框（`:focus` / `:disabled`）、状态栏（`.busy` / `.idle`）、弹层（`.permission` / `.plan`）。
- [x] 静态样式在加载时预计算，绘制零分配；转录行角色惰性计算。
- [x] 自动化测试：`theme` 4 项 + `tui` 若干项（TestBackend 断言缓冲区文本与 truecolor；
      后续增长到 30 项，见第 11 节）。

### 验证
- `cargo test -p harn-cli --lib` → 14 passed。
- `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- `cargo test --workspace` 全绿（search 71 / CLI 集成 17 / provider HTTP 2 / agent 1 / 其余单元）。
- PTY 手工冒烟 `scripts/tui_smoke.py`：面板/输入/状态栏/转录行/空闲↔运行态/token 计数均正确渲染，
  truecolor 序列 `38;2;R;G;B` 与各角色色值一致；用户覆盖主题按预期替换色值。
- PTY 手工冒烟 `scripts/tui_modal_smoke.py`：default 模式下弹层渲染，回车「允许一次」后写入执行成功。
- TUI 纯渲染路径由 TestBackend 测试覆盖，PTY 交互路径由冒烟脚本驱动（见第 11 节扩展）。

---

## 11. 工具可用性 / UI 完整度 / 稳定性与性能（2026-11-07）

### 已完成
- [x] 任务清单工具：`TaskCreate` / `TaskUpdate` / `TaskList`（`harn-tools/src/tasks.rs`，
      会话级 `TaskList`，状态 ○/▶/✓，上限 64，blockedBy 链接）。
- [x] 提问工具：`AskUserQuestion`（`harn-tools/src/ask.rs`，1-4 问、每题 2-6 选项、
      多选；经 `QuestionChannel` 送达 UI；无交互界面时明确告知模型自行决策而非挂起；
      等待响应可被 Ctrl-C 取消，600s 超时兜底）。
- [x] TUI 提问弹层：单选回车确认、多选空格勾选、`Esc`/末项取消；与权限/计划弹层
      共用一套 `Modal` 机制（选项数动态，弹层高度自适应）。
- [x] TUI 侧栏新增任务进度段（与 Agent 共享同一 `TaskList`，`TaskCreate/TaskUpdate` 即时刷新）。
- [x] REPL 降级通道：编号/文本选择器读 stdin 应答提问（非交互无头模式仍走文本指引）。
- [x] `build_registry` 改为 `RegistryDeps`（team_concurrency + tasks + questions），
      doctor/app/manager/子代理全部接线；子代理共享会话级任务清单。
- [x] 性能：Agent 追加消息时维持「连续同角色已合并」不变量，`ProviderRequest.messages`
      改为 `Arc<Vec<Message>>` 跨回合零拷贝共享，providers 不再重复归一化；
      转录渲染缓存（`App::cache`）仅在失效时重建。
- [x] 稳定性：workspace clippy 启用 `unwrap_used` / `expect_used`（`clippy.toml` 测试内豁免，
      集成测试 helper 文件级豁免），生产路径错误一律沿 `Result` 传播；
      `grep.rs` 5 处 expect 降级为安全分支，tui 缓存 expect 消除。
- [x] PTY 冒烟扩展：`scripts/tui_full_smoke.py` 增加 AskUserQuestion 端到端场景
      （弹层渲染 → 回车选择 → 模型继续）。

### 验证
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets`：0 warning（新 lint 下全绿）。
- `cargo test --workspace`：182 passed / 0 failed（search 71 / TUI 30 / CLI 集成 17 /
  provider HTTP 2 / 其余单元）。
- `python3 scripts/tui_full_smoke.py`：26/26 passed（含提问弹层场景）。

### 已知限制（下一步候选）
- 自动上下文压缩未实现（仅手动 `/compact`）。
- 任务清单暂未持久化到会话（重启后从零开始）。
- 多选提问的「跨问题汇总」只回传文本，UI 不记住上一题选择。
- Fork 模式字节级上下文继承未实现。

---

## 12. Claude Code 权限系统移植（2026-11-07）

### 已完成
- [x] 规则来源分层：`harn-config` 的 `RuleSource`（policy > user > project > local >
      flag/cliArg > session）+ `RuleLayer` 分层规则；`Settings::load` 按层标注来源，
      新增企业管控层（`/etc/harn/managed-settings.json` 或 `$HARN_POLICY_FILE`）。
- [x] 转义规则解析（对齐 `permissionRuleParser`）：`\(`/`\)`/`\\` 转义、未转义括号定位、
      畸形输入回退工具名、`Bash()`/`Bash(*)` 视作工具级规则。
- [x] Bash 子命令分解匹配（对齐 `splitCommandWithOperators`，保守策略）：
      引号/转义/子 shell 感知，allow 规则须覆盖每个子命令、deny 命中任一子命令即拒。
- [x] 结构化决策理由 `DecisionReason`（rule[来源] / mode / hook / bashDanger / plan /
      allowlist / userChoice / autoApprove），`PermissionDecision::reason()` 输出可读摘要，
      拒绝理由直达模型（tool_result）与用户（转录/TUI）。
- [x] 遮蔽检测（对齐 `shadowedRuleDetection`）：`RuleSet::shadowed()` + policy warnings，
      `harn doctor` 与 TUI 启动提示。
- [x] 测试：rules 8 项（转义/覆盖/遮蔽/分层）+ policy 8 项（子命令走私/全覆盖 allow/
      子命令 deny/policy 压 allow/遮蔽警告/子命令拆分）。

### 验证
- 端到端：user allow `Read` + policy deny `Read(/etc/**)` → `⊘ Read ... 命中 deny 规则
  Read(/etc/**) [policySettings]`；allow `Bash(git status:*)` + deny `Bash(rm:*)` 下
  `git status && rm -rf build` 被 deny 拦截；doctor 打印遮蔽 WARN。
- `cargo fmt --all -- --check` exit 0；`cargo clippy --workspace --all-targets -- -D warnings` exit 0；
  `cargo test --workspace` 194 passed / 0 failed；PTY 冒烟 26/26。

### 已知限制（下一步候选）
- PermissionUpdate 落盘（`/permissions` 命令、AllowAlways 写 settings）尚未实现（task #17）。
- `~` 路径展开、glob 间包含关系的遮蔽判定暂保守（宁可漏报）。
- 自动上下文压缩未实现（仅手动 `/compact`）。
- 任务清单暂未持久化到会话。

---

## 13. CC 移植第二阶段：状态栏 / TodoWrite / color-diff / 权限更新（2026-11-07）

### 已完成
- [x] TUI 状态栏 CC 化：模式徽标（`⏵⏵ bypass 已开启` 等）、tok、agents、
      运行中 activeForm、shell 计数、RSS（/proc/self/status）、pid；宽度预算降级。
- [x] Shift+Tab 快捷键循环切换六种权限模式（TUI 内即时生效 + banner）。
- [x] `TodoWrite` 工具（`harn-tools/src/tasks.rs`）：整体替换语义
      （content/status/activeForm/blockedBy），`TaskList::replace_all` / `active_form`。
- [x] 新 crate `harn-diff`：移植 CC `color-diff-napi` 纯算法——jsdiff 兼容
      tokenize/Myers 引擎（lines+words，含 jsdiff 平手裁决与 ±1 对角收缩）、
      `structured_patch` hunks（`\ No newline` 标记）、bat 风格 ANSI 渲染
      （行背景 + word-diff 背景 + 行号 + truecolor/256/an 模式）。
- [x] `Edit` / `Write` 成功输出附带 bat 风格 diff（端到端验证：del 红底/add 绿底）。
- [x] PermissionUpdate：`PermissionPolicy::apply_update`（AddRule/RemoveRule/SetMode，
      destination = session/user/local）+ settings 文件读-改-写（`harn-config`，
      保留其他字段）+ 落盘后从磁盘重载（保留来源分层）。
- [x] `/permissions` 命令：list / allow / ask / deny / remove / mode
      [--user|--local|--session]，REPL 与 TUI 双实现。
- [x] TUI 权限弹层新增「始终允许并写入本地设置」选项（4 项，落盘 local settings）。

### 验证
- `cargo fmt --all -- --check` exit 0；clippy `-D warnings` exit 0；
  `cargo test --workspace` 206 passed / 0 failed（harn-diff 7 / permission 28 /
  TUI 32 / CLI 集成 17 / search 71 / 其余）。
- PTY 冒烟 26/26；Edit diff 着色无头端到端（48;5;52 del / 48;5;232 add）。
- Policy 分层端到端：user allow + policy deny → `命中 deny 规则 Read(/etc/**) [policySettings]`。

### 已知限制（下一步候选）
- harn-diff 未含 syntect 语法高亮（重依赖，暂缓）；着色仅 diff 语义。
- 自动上下文压缩未实现（仅手动 `/compact`）。
- 任务清单/TodoWrite 未持久化到会话。

---

## 14. 编排契约 / 工具移植 / crate 合并（2026-11-07）

### 已完成
- [x] 编排契约（修复多 Agent 结构性缺陷）：
  - `SubagentOutcome` 结构化终态（Success/Failed/Cancelled/NeedsHuman/Blocked/Escalation），
    **空返回按失败处理**，tool_result 带 `[TAG]` 前缀。
  - 委派契约：Agent 工具 `dependsOn`（依赖非 Completed → `[BLOCKED]` 不发出）、
    `deadlineSeconds`（绝对 deadline，超时显式失败并取消在途请求）、
    `requiredTools`（子代理工具集不含 → `[ESCALATION]`，不静默降级）。
  - Team 工具 `dependsOn` 拓扑分批：只有全部依赖 SUCCESS 的任务才启动，
    无可启动批次时剩余任务显式 BLOCKED（不再整批并行扇出）。
  - 生命周期：子代理本就持有独立 AbortSignal；新增状态落盘
    （`<session>/agents/<id>/status.json`）+ `load_persisted_subagents()`。
  - 子代理系统提示三条硬指令：直接执行不复述、只报告已验证内容（未验证标 NOT DONE）、
    范围不清则停下请求人工决策；空输出=失败。
- [x] 工具移植：`WebFetch`（reqwest + HTML→文本 + SSRF 防护 + 内容类型/大小限制）、
      `NotebookEdit`（nbformat 4.x cell replace/insert/delete）。
- [x] crate 合并：`harn-diff` 并入 `harn-tools::diff`（仅 Edit/Write 使用），
      删除独立 crate 与 workspace 依赖。

### 验证
- fmt exit 0；clippy `-D warnings` exit 0；`cargo test --workspace` 222 passed / 0 failed
  （diff 7 / permission 28 / TUI 32 / tools 25 / CLI 集成 17 / search 71 / 其余）。
- PTY 冒烟 26/26。

### 已知限制（下一步候选）
- 落盘子代理状态尚未在 TUI 启动时恢复展示（仅磁盘可查）。
- TUI 视觉重设计进行中（用户反馈当前页面不满）。

## 15. TUI 视觉重设计 + 质量债修复（2026-11-07）

用户反馈原话：「全是单调的颜色」「重点突出你都做不好」「你的字也太小了」
「我说一个东西它会随着状态变化，你就只会给一个纯红、纯白」。本轮按
Claude Code 源码（`src/utils/theme.ts` darkTheme、`PermissionMode.ts` getModeColor、
`UserPromptMessage.tsx`、`PromptInput.tsx`、`PromptInputFooterLeftSide.tsx`）
逐项移植视觉语言，并把两个遗留验证疑点修掉。

### 已完成
- [x] 色板换为 CC 暗色板：正文纯白、用户行底色 `#373737`、bash 底色 `#413c41`、
      品牌橙 `#d77757`、成功 `#4eba65`/错误 `#ff6b80`/警告 `#ffc107`、
      plan `#48968c`/accept `#af87ff`（值域对齐 `getModeColor`）。
- [x] **随状态变化**：模式徽标六色（bypass/dontAsk 红、plan 青、accept 紫、auto 琥珀）；
      输入框底圆角线 `╰──╯` 描边随模式换色；占位提示按模式给行动指引；
      状态栏 `● running` 琥珀、activeForm 蓝紫粗体、RSS 三级变色（512/1536MB 阈值）；
      流式呼吸色（品牌橙↔蓝紫 8 帧三角波，`App.tick` 250ms 节拍）；
      侧栏进行中任务 braille spinner 旋转。
- [x] **重点突出**：用户消息整行色块补满宽度 + 粗体；工具名粗体、状态标记 dim；
      欢迎首行品牌橙粗体；转录全宽流式无外框 + `↑ N more` 滚动角标。
- [x] **质量债 1**：`render_unified` 拍平多 hunk 导致第二个 hunk 行号全错 →
      新增 `render_unified_hunks` 按 hunk `old_start/new_start` 起算（对齐 CC
      `TsColorDiff.render`），回归测试覆盖。
- [x] **质量债 2**：`reload_from_disk` 只重读 user/local → `/permissions` 落盘后
      project/policy 层 deny 消失（权限提升风险）→ `with_home` + `Settings::load`
      全量分层重载，回归测试 `reload_keeps_project_layer_deny`。

### 验证
- fmt exit 0；clippy `-D warnings` exit 0；`cargo test --workspace` 227 passed / 0 failed。
- PTY 冒烟 25/25（脚本修正：press() 曾把重绘输出读进 window 却忘记并回 chunks，
  导致累积断言永远停在首帧）。
- PTY 视觉探针：前景色 10 种（模式紫/成功绿/品牌橙/琥珀/呼吸三档/灰阶层级），
  用户行与状态栏底色块命中，粗体斜体序列在帧中。

### 已知限制（下一步候选）
- 落盘子代理状态尚未在 TUI 启动时恢复展示（仅磁盘可查）。
- TUI 无语法高亮（CC 用 highlight.js；可评估 syntect）。
- 呼吸动画受 250ms 事件节拍限制，帧率偏低。

## 16. 会话恢复：落盘子代理状态回填 TUI（2026-11-07）

### 已完成
- [x] TUI 启动时读取 `<session>/agents/*/status.json`（`load_persisted_subagents`），
      把上次会话的子代理记录作为 `Entry::Subagent` 回填转录与侧栏 agents 区。
- [x] **诚实化状态**：落盘为 `running` 的记录在进程重启后不可能还活着，
      恢复时映射为 `cancelled` 并在描述追加「（上次会话中断）」——避免侧栏挂出
      永不结束的假 spinner。已收尾的 Completed/Failed/Blocked/Cancelled 原样恢复。
- [x] 恢复非空时给出 `restored N subagent record(s) from disk` 提示。

### 验证
- 单元测试：`restore_persisted_subagents_marks_interrupted_running`（running→cancelled
  + 描述标注，completed 原样）、`restore_persisted_subagents_without_files_is_noop`。
- 端到端：mock 跑出子代理 → 落盘 status.json → `--tui --resume <session>` 启动，
  PTY 断言侧栏 `agents (1)`、`restored 1 subagent`、`completed` 均出现。
- fmt/clippy 干净；`cargo test --workspace` 229 passed / 0 failed；PTY 冒烟 25/25。

### 已知限制（下一步候选）
- 恢复的记录只读展示；未提供「重新附加/续跑中断子代理」的能力。
- TUI 无语法高亮（CC 用 highlight.js；可评估 syntect）。
- 呼吸动画受 250ms 事件节拍限制，帧率偏低。
## 17. LSP 集成：语言服务器接入 Agent 循环（2026-11-07）

移植 Claude Code `src/services/lsp`（LSPClient / LSPServerInstance / LSPServerManager /
LSPDiagnosticRegistry + LSPTool 九操作）到 `harn-tools::lsp`。没有拆新 crate——
LSP 是工具层能力，作为 harn-tools 的模块（`client` / `instance` / `manager` /
`diagnostics` / `format` / `tool`），配置类型放 harn-config 的 `lspServers` 段。

分层与共享：`RegistryDeps.lsp → RuntimeDeps.lsp → Agent.lsp → ToolContext.lsp`
（None = 未配置，工具返回配置引导）；主代理与子代理共享同一个 LspManager 实例。

相对 CC 的更新：
1. `lsp-types` 0.97（LSP 3.17）+ `url` 桥接（0.97 的 `Uri` 是 fluent-uri newtype，
   没有 `from_file_path`/`to_file_path`）；
2. **位置编码协商**：initialize 声明 `positionEncodings: [utf-8]`，服务器声明
   utf-8 时行列按 Unicode scalar 传（rust-analyzer 路线），否则按 UTF-16 code unit
   折算（LSP 3.17 默认，pylsp 路线）——CC 未实现，多字节字符旁跳转偏移；
3. 配置经 settings 六层分层（user/project/local…），不再限于插件通道；
4. `documentSymbol` 同时接受 `DocumentSymbol[]`（树）与 `SymbolInformation[]`（扁平）
   两种合法返回形态——真实流量抓到 pylsp 返回后者，原先只解树形会静默输出空
   （`tool.rs::tests::document_symbol_accepts_symbol_information_shape` 回归钉死）。

验证：
- mock server（`tests/mock_lsp_server.py`，~230 行完整 LSP 协议实现）走全链路：
  initialize 握手、UTF-8 协商、didOpen→publishDiagnostics、九类请求、shutdown；
- **真实第三方服务器 pylsp 1.15.0**（--ignored 测试 + mock-provider 端到端）：
  goToDefinition 精确返回 `app.py:4:5`、hover 完整签名+docstring、references 两处、
  documentSymbol 双符号、UTF-16 回退路径验证、workspaceSymbol 不支持时透传
  Method Not Found；
- rust-analyzer 1.99.1 在本机（Android/aarch64 沙箱）启动线程但不回 initialize
  （strace 确认进程存活无输出），属环境限制，协议层无差异；
- `HARN_LSP_TRACE=<file>` 帧级 trace（默认关闭）——真实服务器的两个 bug 均由它定位。

### 已知限制（下一步候选）
- documentSync 只做全量 didChange，无增量（2.0 ranges）；
- 崩溃回调已接通 JsonRpcClient，但 LspManager 的重启路径未接（手动重新查触发）；
- 诊断已收集进 DiagnosticRegistry，但未接入 PostToolUse 被动反馈（CC 的
  passiveFeedback）；下一步：编辑成功后若该文件有 error 级诊断，追加提示。
- 无 `/lsp` 状态子命令（已启动服务器/协商编码/诊断计数）。
## 18. 工作区隔离：子代理 git worktree 管理（2026-11-08）

移植 Claude Code 的 `EnterWorktreeTool`/`ExitWorktreeTool` + `src/utils/worktree.ts`。
没有拆新 crate：`harn-tools::worktree`（mod/git/tool 三文件，~600 行）+ settings
顶层 `worktree` 段 + 装配沿 LSP 同一条注入路径（RegistryDeps → RuntimeDeps →
Agent → ToolContext）。

关键设计：
- 工作区在**项目内** `.harn/worktrees/<slug>`（分支 `worktree-<slug>`，基线默认
  HEAD，不 fetch origin 避免凭据挂起）；slug 校验（段内 `[a-zA-Z0-9._-]`、拒 `..`、
  ≤64、`/`→`+` 展平防 git D/F 冲突）先于一切副作用；非 git 仓库明确报错；
- **双通道**：`Agent(isolation:"worktree")` 派发即隔离（只读类型显式拒绝）+
  会话级 `EnterWorktree`/`ExitWorktree`（Agent 持 WorktreeSlot，execute_one 的
  ToolContext.cwd 随槽位切换；子代理在 BLOCKED_TOOLS 里禁掉这两个）；
- **沙盒不新造机制**：子代理 `Agent.cwd` 与 `PermissionPolicy.child_with_cwd()`
  一起换成 worktree——"工作区内自动放行"、规则目标归一全部以 worktree 为根；
- 收尾语义（对齐 CC）：零改动 → 自动删除；有改动 → keep + 改动计数 + git
  cherry-pick/merge 命令建议（harn 不自动执行合并）；
- `SubagentSpec` 替代 `spawn()` 的 7 个位置参数（全库 4 处调用方）；子代理提示
  注入工作区须知（移植 forkSubagent notice）；status.json 持久化 worktree_path；
- 会话结束（SessionEnd 前）report_worktrees 汇总存活工作区。

真实流量抓到的两个问题（都由测试钉死）：
1. **并发共享 slug 的误导性汇报**：两个子代理同 slug 时先结束者删除工作区，
   后结束者的 finish 在已删目录上 git status 失败 → 曾报"已保留+空命令"；
   修：`finish()` 检测 path 不存在直接返回空统计，fallback 的 kept=false
   （`worktree_e2e.rs::concurrent_shared_slug_finish_is_not_misleading`）；
2. **只读子代理 + isolation 的静默接受**：在 Agent 工具层显式 ESCALATION 拒绝
   （`agent.rs::tests::isolation_worktree_rejected_for_read_only_subagents`）。

验证：8 个 worktree_e2e 集成测试（真实 git init 仓：create/resume/changes/keep/
remove fail-closed/back-to-back 三轮/resume-after-changes/并发共享 slug/非 git 报错）；
mock-provider 真实 git 项目端到端：子代理 Write 落 `.harn/worktrees/real-feature/notes.txt`
（主工作树无此文件）、finish 报 1 个未提交文件 + 两条合并命令。

### 已知限制（下一步候选）
- 未自动执行合并（只给命令；MergeWorktree 是下一步候选）；
- 无 TUI 工作区视图/`harn worktree` 子命令；
- 未做 .worktreeinclude/Husky core.hooksPath/WorktreeCreate hook（CC 有，均为可选增强）；
- settings.symlinkDirectories/sparsePaths 已实现但只有集成测试之外的渠道可触发。

### MergeWorktree：自动合并（2026-11-08，验证 PASS 之后的小轮）

`MergeWorktreeTool` + `WorktreeManager::merge_plan/merge_into`：把隔离工作区的
**已提交内容**合并回主工作树当前分支（`cherry-pick base..branch` 默认，或
`merge --no-ff`）。设计要点：

- `dryRun=true` 只出提交列表与前置检查（未提交文件数、两侧脏否），零副作用；
- 冲突**自动回退**（`cherry-pick --abort` / `merge --abort`），主树绝不留半合并
  状态——冲突判定不依赖 git 英文输出（locale 可能是中文），用
  `.git/CHERRY_PICK_HEAD`/`MERGE_HEAD` 存在性 + `.gitignore` 无关的状态文件；
- 主工作树有未提交改动时拒绝合并（保护用户"在途工作"）；
- base 落盘在工作区内 `.harn/worktree-base`（**在 worktree 里 rev-parse HEAD
  拿到的是分支自身**，`HEAD..branch` 恒为空——这是 mock e2e 抓出来的真 bug；
  base 还同时排除 `.harn` pathspec 外的统计，`git status` 不再把 harness 自管
  目录算成用户脏文件）；
- create 时幂等写主仓 `info/exclude` 的 `.harn/worktrees/`：否则用户
  `git add .` 会把 worktree 目录（内部嵌套 .git）当嵌入式仓库提交进历史。

测试：4 个新 e2e（干净合并落地 / 冲突回退且主树内容不变 / 主树脏拒绝 /
零提交拒绝）+ 原 12 个 worktree e2e 全绿；mock 真实 git 仓验证
子代理 commit → MergeWorktree 落地 `feat: new_fn` 至主分支。

## 20. 对话 Markdown 渲染：移植 CC 完整视觉（2026-11-08）

不新拆 crate：`harn-ui` 长 `markdown.rs`（MD→RenderLine）与 `highlight.rs`
（syntect 封装），theme CSS 加类，tui.rs 按 spans 消费。依赖新增
pulldown-cmark/syntect/strip-ansi。移植 CC `src/utils/markdown.ts`（468 行核心）
的全部呈现规则；不做 OSC8 真超链接（ratatui 切 cell 会断转义序列，链接用
样式 + href 文本）；流式全量重解析（不需要 CC 的 stable-prefix）。

抓修的真 bug（都有回归测试）：
1. **transcript 的 truncate 把换行压成空格**：ToolEnd 的 result 一律变单行，
   多行 diff 在入口即被毁——改为保留换行的字符截断
   （`transcript.rs::truncate`，E2E 见 tui.rs diff 测试）；
2. **Edit/Write diff 的 ANSI 串在 TUI 逐字乱码**：`render_diff_marked` 产
   标记化文本（行首 `+`/`-`/` ` + `[n]` gutter），render.rs `diff_rows` 识别上
   diff-add/del/gutter 底色；渲染入口统一 strip_ansi 兜底；
3. **quote/list 前缀只进 text 不进 spans**：TUI 优先渲染 spans，前缀丢失——
   flush_line 把 indent/quote 前缀并入 spans。

测试：harn-ui 41（markdown 11 + highlight 6 + diff_rows 4 + 原有）、harn-cli
TUI TestBackend 视觉断言 4（MD 无记号/表格边框/高亮色命中/diff 底色）+ 全量
304 passed；PTY 冒烟 25/25。syntect scope→类别映射的关键事实：Rust 的 `fn` 是
`storage.type.function`（不是 keyword）、宏是 `support.macro`——映射表含这两类。

### 收尾轮（验证残余观察）：autolink 去重 + plain MD 化

- autolink `<url>` 不再渲染成 `url (url)`（对齐 CC #10283）；
- PlainUi（REPL/headless 共用）流式 delta 进缓冲，段落结束时经
  `render_text_block` 按 MD 一次出块——`-p` 输出结构化（标题/引用/表格/
  代码块/diff 行上底色），保留逐 token 直出的旧开关。
- headless e2e 实测：`# 方案` →「方案」、`**要点**` →「要点」、table box、
  diff `+[1] hello` 均在位；TUI 冒烟 25/25；全量 314 passed。

### 去第三方依赖第一轮：inquire → 自研选择器 + AskUserQuestion 表单

- `harn-permission/src/select.rs`（新）：纯状态机 `next_move`/`advance`/
  `render_lines`（33 测试）+ crossterm IO 壳 `select_blocking`（RawGuard、
  覆盖重绘、Esc/Ctrl-C → None）。`prompter.rs` 四模态权限 + 三模态计划审批
  全部换用；`inquire` 依赖删除。
- TUI 弹层移植 CC AskUserQuestion 形态：`N. label` 序号 + 两行式描述、
  数字键 1-9 直选、`Other…` 合成项（第一次 Enter 进编辑态、自由文本作答案、
  多选与勾选项合并）、`preview` 块（选中项带预览时展示，20 行裁剪）。
  `QuestionOption` 增加 `preview` 字段并入 schema。
- 顺带修掉冒烟稳定失败的根因（见 20f67a4）：diff 结果前导摘要行被吞。
- 测试：harn-cli question modal 6 项（序号/两行式/Other 编辑/preview/
  回车/取消/多选勾选）；全量 27 测试集绿；PTY 冒烟 25/25 × 3 轮稳定。
- 已知小坑：ratatui TestBackend 对 CJK 宽字符在 cell 间插空格，断言需
  去空白后比对。

### i18n 基础设施 + 斜杠命令弹层（CC 式带介绍面板）

- 新 crate `harn-i18n`：rust-i18n 引擎 + `SimpleBackend` **运行时**加载
  （非编译期 embed，加语言免重编）；`tr!(key, name = v)` 宏做 `{name}` 插值。
- 语言包统一放仓库根 `locales/{zh,en}.json`（用户要求：不散落在 crate 里）；
  目录解析 `HARN_LOCALES_DIR` > exe 相对 > 源码树相对。查找链：当前语言 →
  兜底 zh → key 本身；settings 增 `locale`（四层 merge），CLI 启动解析
  （配置 > HARN_LOCALE/LC_ALL/LC_MESSAGES/LANG > zh）。
- `CommandDef` 表（name/usage/description_key/aliases）取代裸名表：
  补全弹层与帮助浮层单一数据源；描述全部走 i18n key。
- 补全行为移植 CC `generateCommandSuggestions`：裸 `/` 出全集、唯一匹配
  也显、排序 精确名>前缀名(短优先)>精确别名>前缀别名>子串、别名补全为
  规范名（/qu→/exit）、带参输入自动收起；弹层行 = 名称列对齐(≤40% 宽) +
  dim 描述列 + CJK 宽度截断 + 6 行居中滚动窗口。
- 冒烟脚本升级为**终端屏幕模拟器**（CSI CUP/CU*/ED/EL + 宽字符续格）：
  ratatui diff 渲染下相同前缀单元格不重印，朴素拼接会把 "/compact" 断成
  碎片（旧 "补全被接受" 断言一直是字节流假阳——实际接受的是短名优先的
  /cost）；25 项断言改为对最终屏幕判定，3 轮 25/25 稳定。

### 统一 User-Agent + 真实模型闭环测试（zcode 端点）

- `harn-providers` 出站客户端统一 `user_agent(harn-cli/<ver> (external, cli))`
  （lib.rs 常量，形态对齐 CC）；http.rs 的 serve_once 改为捕获请求头，
  新增 UA 断言。
- 真实模型 e2e（https://zcode.aisenno.com/v1，GLM-5.3-Flash）三场景全通：
  1. **单工具闭环**：Read hello.txt → 逐字复述 `hello-zcode-42`。
  2. **多工具闭环**：Write out.txt（8 字节、diff 前导行可见）→ Read 回读
     → 引用工具结果细节作答。
  3. **自主迭代**：写 sum.py → Bash 运行报错（stderr: nums.txt 不存在）
     → 自主 `seq 1 10` 建文件 → 再跑出 55 → 并解释 10×11/2=55 验算。
  4. **图像识别**：--image 传入 PIL 生成的夕阳山景图，模型准确描述
     暖色渐变天/右上太阳/三座紫灰山/深藏青水面，并判定为“黄昏日落
     极简扁平插画”风格——视觉链路（image → base64 → vision）全通。
- 网络注意：Wikimedia 直连被墙（curl 35），改用 PIL 本地生成测试图。

### Web UI 第一阶段：协议 + 自研服务 + E2E 闭环（DSH/jcode 形态）

- **harn-web-protocol**（新 crate）：wasm-clean 共享 DTO（serde-only）。
  `ServerMsg`（Entry/Delta/Modal/Status/Done/Error）× `ClientMsg`
  （Send/Abort/Choose/SetMode/SetLocale），roundtrip 双测。
- **harn-web**（新 crate）：全自研，零 HTTP 框架——
  - 极简 HTTP/1.1 路由（`/`静态、`/ws`升级、405/404/版本校验）；
  - RFC6455 服务端（握手 SHA1+GUID、掩码帧解析、ping/pong、8MiB 上限），
    17 单测（含 RFC 范例 accept-key、扩展长度、无掩码拒绝）；
  - 启动生成 32 字节 token，WS 握手 query 校验；dist 内嵌托管（占位页）。
- **harn-cli::web**：`--web/--web-port` 启动口 + 桥接——AgentEvent → 协议、
  UiAsk/UserQuestion → Modal（oneshot 等浏览器 Choose）、turn 结束广播 Done。
  权限/提问弹窗走 `WebPrompter`（与 TuiPrompter 同型）。
- **E2E（scripts/web_e2e.py，2/2）**：mock provider 下验证
  ① 基本闭环（snapshot→delta→tool→tool_result→done）
  ② 权限闭环（Write 触发 modal → 回 allow_once → 工具真实落盘 out.txt）。
- 定位：这是 daemon（#53）的 HTTP/WS frontend；TUI/Web 为同一 agent loop
  的两个 frontend，协议层后续直接复用到 UDS 帧。

### harn serve daemon（UDS 单服务器多客户端 + exec 热重载）

参考 jcode（1jehuang/jcode）的 SERVER_ARCHITECTURE 落地：
- **harn-daemon**（新 crate）：
  - `uds.rs`：UDS 服务端，**换行分隔 JSON** 帧；多客户端订阅广播
    （broadcast，慢客户端报「落后 N 条」而非静默丢）；控制帧
    server_reload/server_stop 单独通道；socket 权限 0600；空闲 300s
    看门狗（每 10s 巡检，无连接自动退）；
  - `driver.rs`：**传输无关的会话驱动**——AgentEvent→协议、
    PermissionAsk/UserQuestion→Modal（oneshot 等 Choose 回流）、
    Abort→AbortSignal、turn 结束广播 Done。被 UDS/WS 前端共用；
  - `registry.rs`：`~/.harn/servers.json`（名字/socket/PID/启动时间），
    按 PID 存活剔除陈旧项；随机形容词命名（blazing/frozen/vigilant…）。
- **harn-cli::server**：`harn server start/stop/status/reload`：
  - start：注册表探活 → 监听 → 建 Runtime（DaemonPrompter 走协议弹窗）
    → serve 主循环；
  - reload：serve 返回前**不删 socket 文件**——主监听 fd 先 `F_DUPFD`
    复制（serve 返回时原 listener 被 drop），再 `exec()` 同进程换镜像，
    新进程用 `--socket-fd` 接管（同 PID、新随机名、同 socket 路径）；
  - stop：进程内显式 exit(0)（空闲看门狗是常驻任务，runtime drop 会
    等它）。
- **踩坑实录**（测试驱动发现）：
  1. 控制客户端写完不读就关 → 客户端带未读数据 close → 内核 RST →
     控制帧被一起丢弃（RST 竞态）→ control() 写后 drain；
  2. serve 在 Reload 返回前删 socket 文件 → exec 后新进程 fd 还在但
     路径入口没了 → 连接全失败 → Reload 不删，仅 Stop/超时删；
  3. control 帧参数误传裸字符串 `"server_stop"` 而非 JSON。
- **scripts/daemon_e2e.py**：start→协议闭环→status→reload（断言同 PID
  换名、协议仍可用）→stop（退出码 0）全绿。

下一步（#52）：daemon 的 Web frontend（复用 harn-web，广播→WS 下行、
WS ClientMsg→驱动命令通道），以及 Dioxus 前端本体。

### daemon 的 Web frontend 挂载

`harn server start --web [--web-port]`：daemon 同时在 loopback 暴露
WebSocket（浏览器直达）。桥接方式：broadcast 事件流 → mpsc 转接喂给
harn_web::serve 的 WebSession；WS 收到的 ClientMsg 直进驱动命令通道。
E2E（mock）：daemon --web 下 WS 协议闭环 + Write 权限弹窗 →
allow_once → 真实落盘 out.txt 全通——daemon 成为 TUI/UDS/浏览器
三端共享的单一 runtime。

### Web UI 第二阶段：Dioxus 前端本体 + dist 嵌入（已由下面的 assistant-ui 换栈取代）

- **harn-web-frontend**（新 crate，Dioxus 0.7 → wasm32）：
  - 消息流（user/assistant/thinking/tool/tool_result/status/error 全系）
    + 流式 delta 累积气泡（Done 落定）；
  - 权限弹窗（允许一次/总是允许/拒绝）与提问弹窗，Choose 经 WS 回流；
  - 中断按钮（Abort）；缺 token 引导；**断线指数退避重连**（1s→8s，
    daemon exec 热重载/重启时浏览器自愈）；
  - 整 crate `#![cfg(target_arch = "wasm32")]`：native 构建看空 crate，
    日常 `cargo build/clippy/test` 不需要 wasm32 target 与 dx。
- **嵌入打包**：harn-web feature `embed-frontend` + build.rs——跑
  `dx build --release`，把 dist 拷进 OUT_DIR 生成清单
  （路径/MIME/`include_bytes!`），dist.rs 精确路由 + 目录 index 兜底。
  **wasm 资源以 `application/wasm` 提供**（模块加载必需）；未启用
  feature 时回落占位首页。
- 验证（沙箱无浏览器，前端 DOM 运行时未做 e2e）：
  - 静态层：`/` 200、JS 57KB、wasm 1.95MB 以 `application/wasm`；
  - 协议层：web e2e 2/2 + 静态首页断言；
  - 运行时交互（点击发消息/审批）留给真实浏览器。
- 发布构建须 `--features harn-web/embed-frontend`（需 dx CLI +
  wasm32 target）；常规流水线零额外成本。

### Web 前端换栈第 1 步：脚手架（Dioxus → Vite/React）

- `crates/harn-web-frontend` 变纯 web 工程（pnpm + vite 8 + React 19 + TS +
  tailwind v4）：package.json/tsconfig/vite.config/index.html(id="root")/
  globals.css（oklch 变量 + 系统字体栈 + `@source` 扫描）+ App 空壳；
- **workspace 事实修正**：cargo 的 glob 成员要求含 Cargo.toml——
  `exclude = ["reference", "crates/harn-web-frontend"]`（而非靠 glob 忽略）；
- `harn-web/build.rs` 换轨：dx → `pnpm run build`，产物 `dist/`；
  `pnpm install` 仅在 node_modules 缺失时跑；逃逸 env
  `HARN_WEB_FRONTEND_SKIP_INSTALL` / `HARN_WEB_DIST`；mime_for 补
  `.mjs/.map/.woff2/webp/avif/txt`；dist.rs PLACEHOLDER_INDEX 加
  not(embed) 门；嵌入测试改断言 JS + index.html（新前端无 wasm）；
- 验收：`pnpm run build` → dist（220KB JS + 10.5KB CSS）；
  `cargo build -p harn-cli --features harn-web/embed-frontend` →
  `/` 200、JS `text/javascript` 219954B、CSS `text/css`；web e2e 2/2；
  clippy 干净 + 35 测试集。
- web_e2e 静态断言改为 `id="root" or 占位页`（兼容两种构建）。


### Web 前端换栈第 2-6 步：协议层 → 装配 → 工具卡 → 收尾

- **协议层**（40709f9）：types/ws/id/marked-diff/session 归约器——
  重连即重建、一个 (turn,agentId) 一条助手消息、Tool/ToolResult FIFO
  配对、Modal{Permission} 挂 approval + 显式 requires-action、
  Deny 补合成 result；vitest 流测试复刻 web_e2e 两条剧本；
- **runtime 装配**（2060cdb）：复制区（elements 14 件 + ui/base 5 件，
  thread.aui 裁 attachment）+ HarnRuntimeProvider +
  happy-dom DOM 冒烟；
- **工具卡**（c9e055e）：makeAssistantToolUI 注册 12 个卡（Bash/Read/
  Write/Edit+DiffView/Glob/Grep/WebFetch/TodoWrite/Plan/Agent/Team），
  ToolShell+ApprovalBanner 统一外壳，工具组默认展开；
- **收尾**：i18n 文案层（zh/en + SetLocale 联动 + locale 重挂）、
  CI frontend job（pnpm install/typecheck/test/build）、
  前端 README（开发/打包/复制区同步流程/已知限制）。
- 前端测试 20/20（含 locale 切换、审批点击发 Choose、DOM 渲染）。
