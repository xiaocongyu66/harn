// 归约器流测试：复刻 scripts/web_e2e.py 的两条 mock 剧本，
// 断言 ServerMsg 序列 → messages 终态（无浏览器依赖）。

import { describe, expect, it } from "vitest";

import { SessionStore } from "../session";
import type { ClientMsg, ServerMsg } from "../types";

function harness() {
  const sent: ClientMsg[] = [];
  const store = new SessionStore((message) => sent.push(message));
  return { sent, store, push: (messages: ServerMsg[]) => messages.forEach((message) => store.dispatch(message)) };
}

function assistantMessage(store: SessionStore) {
  const messages = store.getSnapshot().messages;
  return messages.find((message) => message.role === "assistant");
}

function toolCalls(message?: { content: string | readonly unknown[] }) {
  if (!message || typeof message.content === "string") return [];
  return (message.content as readonly { type: string }[]).filter(
    (part) => part.type === "tool-call",
  );
}

describe("SessionStore 基本闭环", () => {
  it("快照状态行 → user → 流式 delta → 工具卡 → 工具结果 → done", () => {
    const { store, push } = harness();
    push([
      { kind: "entry", entry: { kind: "status", text: "harn web · provider=mock" } },
      { kind: "entry", entry: { kind: "status", text: "会话已就绪" } },
      { kind: "entry", entry: { kind: "user", text: "读 note.txt 告诉我内容", images: 0 } },
      { kind: "entry", entry: { kind: "status", text: "thinking" } },
      { kind: "delta", agent_id: "main", text: "我先读一下文件" },
      { kind: "delta", agent_id: "main", text: "再回答。" },
      {
        kind: "entry",
        entry: { kind: "tool", agent_id: "main", name: "Read", input: { file_path: "note.txt" }, status: "running" },
      },
      {
        kind: "entry",
        entry: { kind: "tool_result", agent_id: "main", name: "Read", output: "hello", is_error: false, ms: 0 },
      },
      { kind: "delta", agent_id: "main", text: "文件内容是 hello。" },
      { kind: "done", reason: "completed" },
    ]);

    const state = store.getSnapshot();
    // 状态行不入线程。
    // 快照 2 行 + thinking 1 行，均不入线程。
    expect(state.statusLines).toHaveLength(3);
    expect(state.running).toBe(false);
    // 一条 user + 一条 assistant（一个 turn 一条消息）。
    const users = state.messages.filter((message) => message.role === "user");
    expect(users).toHaveLength(1);
    const assistant = assistantMessage(store);
    expect(assistant).toBeDefined();
    // 流式 delta 合并成一个 text part（工具 part 排在其后）。
    const parts = (assistant?.content ?? []) as readonly { type: string; text?: string }[];
    const text = parts.find((part) => part.type === "text")?.text ?? "";
    expect(text).toBe("我先读一下文件再回答。");
    // 工具 part 配对完成。
    const calls = toolCalls(assistant);
    expect(calls).toHaveLength(1);
    expect((calls[0] as { result?: unknown }).result).toBe("hello");
  });

  it("多帧 delta 追加到同一 text part（id 稳定，defer 增量渲染前提）", () => {
    const { store, push } = harness();
    push([
      { kind: "delta", agent_id: "main", text: "a" },
      { kind: "delta", agent_id: "main", text: "b" },
      { kind: "delta", agent_id: "main", text: "c" },
    ]);
    const assistant = assistantMessage(store);
    expect(assistant?.id).toBe("a-0:main");
    const parts = assistant?.content as readonly { type: string; text?: string }[];
    expect(parts.filter((part) => part.type === "text")).toHaveLength(1);
    expect(parts[0]?.text).toBe("abc");
  });

  it("onNew 发 send 且不乐观插入用户消息（后端回播）", () => {
    const { store, sent } = harness();
    store.sendUser("你好");
    expect(sent).toEqual([{ kind: "send", text: "你好" }]);
    expect(store.getSnapshot().messages).toHaveLength(0);
    expect(store.getSnapshot().running).toBe(true);
    store.dispatch({ kind: "done", reason: "completed" });
    expect(store.getSnapshot().running).toBe(false);
  });

  it("中断：abort 后 done(reason=aborted) 给末条 incomplete/cancelled", () => {
    const { store, push } = harness();
    store.sendUser("跑");
    push([
      { kind: "delta", agent_id: "main", text: "半句" },
      { kind: "done", reason: "aborted_streaming" },
    ]);
    const assistant = assistantMessage(store);
    expect(assistant?.status).toMatchObject({ type: "incomplete", reason: "cancelled" });
  });

  it("重连即重建：reset 清空全部状态", () => {
    const { store, push } = harness();
    push([{ kind: "delta", agent_id: "main", text: "x" }]);
    expect(store.getSnapshot().messages).toHaveLength(1);
    store.reset();
    const state = store.getSnapshot();
    expect(state.messages).toHaveLength(0);
    expect(state.running).toBe(false);
    expect(state.question).toBeNull();
  });
});

describe("SessionStore 权限审批", () => {
  it("工具 entry 先于 modal 到达 → approval 挂到 part，消息 requires-action", () => {
    const { store, push } = harness();
    push([
      {
        kind: "entry",
        entry: { kind: "tool", agent_id: "main", name: "Write", input: { file_path: "out.txt" }, status: "running" },
      },
      {
        kind: "modal",
        modal: { kind: "permission", tool: "Write", target: "out.txt", reason: "默认模式需批准写操作" },
      },
    ]);
    const assistant = assistantMessage(store);
    const calls = toolCalls(assistant) as {
      type: string;
      approval?: { options: { id: string; label: string }[] };
    }[];
    expect(calls[0]?.approval?.options.map((option) => option.id)).toEqual([
      "allow-once",
      "allow-always",
      "reject-once",
    ]);
    expect(assistant?.status).toMatchObject({ type: "requires-action" });
    expect(store.getSnapshot().pendingApproval).toBeDefined();
  });

  it("允许一次 → 发 choose + 乐观置 approved=true", () => {
    const { store, sent, push } = harness();
    push([
      {
        kind: "entry",
        entry: { kind: "tool", agent_id: "main", name: "Write", input: {}, status: "running" },
      },
      { kind: "modal", modal: { kind: "permission", tool: "Write", target: null, reason: "r" } },
    ]);
    const approvalId = store.getSnapshot().pendingApproval!;
    store.respondApproval(approvalId, "allow_once");
    expect(sent.at(-1)).toEqual({ kind: "choose", reply: { kind: "permission", choice: "allow_once" } });
    const calls = toolCalls(assistantMessage(store)) as { approval?: { approved?: boolean } }[];
    expect(calls[0]?.approval?.approved).toBe(true);
  });

  it("拒绝 → 补合成 result（后端 Deny 不发 ToolResult）", () => {
    const { store, sent, push } = harness();
    push([
      {
        kind: "entry",
        entry: { kind: "tool", agent_id: "main", name: "Write", input: {}, status: "running" },
      },
      { kind: "modal", modal: { kind: "permission", tool: "Write", target: null, reason: "r" } },
    ]);
    const approvalId = store.getSnapshot().pendingApproval!;
    store.respondApproval(approvalId, "deny");
    expect(sent.at(-1)).toEqual({ kind: "choose", reply: { kind: "permission", choice: "deny" } });
    const calls = toolCalls(assistantMessage(store)) as {
      approval?: { approved?: boolean };
      result?: unknown;
      isError?: boolean;
    }[];
    expect(calls[0]?.approval?.approved).toBe(false);
    expect(calls[0]?.result).toBe("已由用户拒绝");
    expect(calls[0]?.isError).toBe(true);
  });

  it("提问弹窗 → answers 回包；多选标记保留", () => {
    const { store, sent, push } = harness();
    push([
      {
        kind: "modal",
        modal: {
          kind: "question",
          question: "选哪个",
          header: "确认",
          options: [{ label: "A", description: null, preview: null }],
          multi_select: true,
        },
      },
    ]);
    expect(store.getSnapshot().question?.question).toBe("选哪个");
    store.respondQuestion(["A"]);
    expect(sent.at(-1)).toEqual({ kind: "choose", reply: { kind: "question", answers: ["A"] } });
    expect(store.getSnapshot().question).toBeNull();
    // 取消（Dismiss）= answers null。
    push([{ kind: "entry", entry: { kind: "status", text: "x" } }]);
    // Question 已关闭，且 status 行正常入流。
    expect(store.getSnapshot().statusLines.at(-1)).toBe("x");
  });

  it("解除选择（Dismiss）", () => {
    const { store, sent, push } = harness();
    push([
      {
        kind: "modal",
        modal: {
          kind: "question",
          question: "继续吗",
          header: "确认",
          options: [],
          multi_select: false,
        },
      },
    ]);
    store.respondQuestion(null);
    expect(sent.at(-1)).toEqual({ kind: "choose", reply: { kind: "question", answers: null } });
  });
});

describe("SessionStore 工具配对", () => {
  it("同名工具多回合 FIFO：结果挂到最老未配对项", () => {
    const { store, push } = harness();
    push([
      { kind: "entry", entry: { kind: "tool", agent_id: "main", name: "Read", input: { file_path: "a" }, status: "running" } },
      { kind: "entry", entry: { kind: "tool", agent_id: "main", name: "Read", input: { file_path: "b" }, status: "running" } },
      { kind: "entry", entry: { kind: "tool_result", agent_id: "main", name: "Read", output: "A内容", is_error: false, ms: 0 } },
    ]);
    const calls = toolCalls(assistantMessage(store)) as { result?: unknown; args?: { file_path?: string } }[];
    expect(calls).toHaveLength(2);
    expect(calls[0]?.result).toBe("A内容");
    expect(calls[1]?.result).toBeUndefined();

    push([
      { kind: "entry", entry: { kind: "tool_result", agent_id: "main", name: "Read", output: "B内容", is_error: false, ms: 0 } },
    ]);
    const calls2 = toolCalls(assistantMessage(store)) as { result?: unknown }[];
    expect(calls2[1]?.result).toBe("B内容");
  });

  it("子代理独立消息（agent_id 区分）", () => {
    const { store, push } = harness();
    push([
      { kind: "delta", agent_id: "main", text: "主" },
      { kind: "delta", agent_id: "explore-1", text: "子" },
    ]);
    const messages = store.getSnapshot().messages;
    expect(messages).toHaveLength(2);
    expect(messages[0]?.id).toBe("a-0:main");
    expect(messages[1]?.id).toBe("a-0:explore-1");
  });

  it("Entry{Error} → 单独错误助手消息（incomplete/error）", () => {
    const { store, push } = harness();
    push([
      { kind: "entry", entry: { kind: "error", text: "Provider 错误：403" } },
    ]);
    const state = store.getSnapshot();
    expect(state.messages).toHaveLength(1);
    expect(state.messages[0]?.status).toMatchObject({ type: "incomplete", reason: "error" });
  });
});
