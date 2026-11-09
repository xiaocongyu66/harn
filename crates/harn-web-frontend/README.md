# harn web 前端

基于 [assistant-ui](https://github.com/assistant-ui/assistant-ui) 的 React SPA：
pnpm + Vite 8 + React 19 + TypeScript + Tailwind v4。后端协议零改动——
通过 `useExternalStoreRuntime` 把 harn 的 WebSocket 协议
（`harn-web-protocol`）归约为 assistant-ui 的消息模型（见
`src/lib/session.ts`）。

## 开发

```bash
pnpm install            # 装依赖
pnpm run dev            # 本地开发（/ws 走代理到 HARN_WEB_PORT，默认 7331）
HARN_WEB_PORT=7332 pnpm run dev   # 指定本地 harn --web 端口
pnpm run typecheck      # 类型检查
pnpm test               # vitest（happy-dom DOM 冒烟）
pnpm run build          # tsc + vite build → dist/
```

开发模式：另起 `harn --web --web-port 7331 --provider <provider>`，
打开 vite 提示的 URL 并把终端打印的 token 拼到 query（`?token=…`）。

## 打包

`harn --web` 通过 `crates/harn-web/build.rs`（feature `embed-frontend`）
调用本目录 `pnpm run build` 并嵌入 `dist/`。打包机可用
`HARN_WEB_FRONTEND_SKIP_INSTALL=1`（自行装好依赖）或
`HARN_WEB_DIST=<path>`（直接用预构建产物）跳过构建步骤。

## 目录结构

- `src/lib/`：协议层（types/ws）与会话归约器（session，**核心逻辑**，
  带完整 vitest 流测试，与 Rust 侧 `web_e2e.py` 的剧本同构）；
- `src/runtimes/`：ExternalStoreRuntime 装配；
- `src/components/assistant-ui/`：**从 assistant-ui `packages/ui`
  复制的主题件**（该包未发布 npm，只能复制；见下方同步流程）；
- `src/components/ui/`：复制的 shadcn 式基础件（`@base-ui/react`）；
- `src/components/harn/`：harn 业务组件（状态栏/弹窗/工具卡）；
- `src/i18n.ts`：文案表（zh/en），复制件里的硬编码字符串一律走 `t()`。

## 复制区的上游同步

`packages/ui`（上游仓库 `assistant-ui/assistant-ui`）未发布 npm，
复制区只能手工同步：

1. 克隆上游，取相应版本（当前基线：`@assistant-ui/react` 0.15.x、
   `@base-ui/react` 1.9.x）；
2. 把 `packages/ui/src/components/react/` 下的 elements/ui 件按需覆盖到
   `src/components/`（**保持路径平移**：`assistant-ui/`、`ui/` 两级目录）；
3. 已做的本地裁剪需要重做：
   - `thread.aui.tsx`：裁掉 attachment（dropzone/附件按钮）相关 import 与
     JSX；
   - `thread.aui.tsx` / `tool-fallback.aui.tsx` 等：用户可见字符串改
     `@/i18n` 的 `t()` 调用（键见 `src/i18n.ts`）；
   - 工具组默认展开在 `src/components/harn/ThreadKit.tsx`（不在复制文件内，
     上游同步不受影响）；
4. `pnpm run typecheck` + `pnpm test` 验证。

## 已知限制

- 同名并发工具的结果按 FIFO 配对（协议未带 `tool_use_id`）；
- 附件/语音/消息编辑等 composer 能力按能力位门控，未接适配器；
- 审批/human-input 以 Inline approval 形态渲染在工具卡上。
