// 工具卡 + 审批 UI 冒烟：WS 灌入 Bash 工具调用与权限弹窗，
// 断言卡片渲染、审批按钮出现、点击拒绝发出 Choose{Permission{deny}}
// 且 Deny 合成 result 呈现。

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { App } from "@/App";

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  static last(): FakeWebSocket {
    return FakeWebSocket.instances.at(-1)!;
  }
  static reset() {
    FakeWebSocket.instances = [];
  }

  url: string;
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: unknown[] = [];

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  send(payload: string) {
    this.sent.push(JSON.parse(payload));
  }

  close() {
    this.onclose?.();
  }

  emit(message: unknown) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
}

describe("工具卡 + 审批 UI", () => {
  beforeEach(() => {
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    globalThis.history.replaceState(null, "", "/?token=tok");
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("Bash 卡渲染命令与输出", async () => {
    render(<App />);
    await waitFor(() => expect(FakeWebSocket.instances).toHaveLength(1));
    const ws = FakeWebSocket.last();
    ws.onopen?.();

    ws.emit({
      kind: "entry",
      entry: {
        kind: "tool",
        agent_id: "main",
        name: "Bash",
        input: { command: "ls -la", description: "列目录" },
        status: "running",
      },
    });
    ws.emit({
      kind: "entry",
      entry: {
        kind: "tool_result",
        agent_id: "main",
        name: "Bash",
        output: "total 1\ndrwxr-xr-x src",
        is_error: false,
        ms: 0,
      },
    });

    await waitFor(() => {
      expect(screen.getByText("ls -la")).toBeTruthy();
    });
    await waitFor(() => {
      expect(screen.getByText(/total 1/)).toBeTruthy();
    });
  });

  it("权限弹窗挂到工具卡：三按钮 + 点击拒绝发 Choose{deny} 并合成 result", async () => {
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(FakeWebSocket.instances).toHaveLength(1));
    const ws = FakeWebSocket.last();
    ws.onopen?.();

    ws.emit({
      kind: "entry",
      entry: {
        kind: "tool",
        agent_id: "main",
        name: "Write",
        input: { file_path: "out.txt", content: "x" },
        status: "running",
      },
    });
    ws.emit({
      kind: "modal",
      modal: {
        kind: "permission",
        tool: "Write",
        target: "out.txt",
        reason: "默认模式需批准写操作",
      },
    });

    // 审批按钮出现（ToolShell 的 ApprovalBanner）。
    const reject = await screen.findByRole("button", { name: "拒绝" });
    expect(screen.getByRole("button", { name: "允许一次" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "总是允许" })).toBeTruthy();
    expect(screen.getByText("默认模式需批准写操作")).toBeTruthy();

    await user.click(reject);

    // 发出了 Choose{permission deny}。
    await waitFor(() => {
      expect(ws.sent.at(-1)).toEqual({
        kind: "choose",
        reply: { kind: "permission", choice: "deny" },
      });
    });
    // 审批落定的回执（ApprovalBanner，工具卡折叠区之外）。
    await waitFor(() => {
      expect(screen.getByText("已拒绝")).toBeTruthy();
    });
  });

  it("未注册工具走 ToolFallback 兜底（不裸 JSON）", async () => {
    render(<App />);
    await waitFor(() => expect(FakeWebSocket.instances).toHaveLength(1));
    const ws = FakeWebSocket.last();
    ws.onopen?.();
    ws.emit({
      kind: "entry",
      entry: {
        kind: "tool",
        agent_id: "main",
        name: "SomethingNew",
        input: { a: 1 },
        status: "running",
      },
    });
    // ToolFallback 至少渲染工具名（有可折叠的 args），不挂掉整页。
    await waitFor(() => {
      expect(screen.getByText(/SomethingNew/)).toBeTruthy();
    });
  });
});
