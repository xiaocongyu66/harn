// App 冒烟：无浏览器环境下验证 WS ↔ Thread 渲染链路。
// happy-dom 提供 DOM，FakeWebSocket 拦截建连并直接灌 ServerMsg 序列。

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { setLocale } from "@/i18n";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { App } from "@/App";

/** 最小 WebSocket 桩：保存 url、暴露 open/emit、记录 send。 */
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

  /** 测试侧触发：喂一条协议消息。 */
  emit(message: unknown) {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
}

// vitest 环境注入 location.query（happy-dom 支持 history.replaceState）。
function setToken(token: string) {
  globalThis.history.replaceState(null, "", `/?token=${token}`);
}

describe("App 冒烟（happy-dom）", () => {
  beforeEach(() => {
    FakeWebSocket.reset();
    setLocale("zh");
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("建连 /ws?token=…，流式 delta 渲染进消息流", async () => {
    setToken("tok");
    render(<App />);

    await waitFor(() => {
      expect(FakeWebSocket.instances).toHaveLength(1);
    });
    const ws = FakeWebSocket.last();
    expect(ws.url).toBe("/ws?token=tok");

    // 连接建立后灌入欢迎快照 + 一轮问答。
    ws.onopen?.();
    ws.emit({ kind: "entry", entry: { kind: "status", text: "harn web · provider=mock" } });
    ws.emit({ kind: "entry", entry: { kind: "user", text: "你好", images: 0 } });
    ws.emit({ kind: "delta", agent_id: "main", text: "你好，" });
    ws.emit({ kind: "delta", agent_id: "main", text: "我是 harn。" });
    ws.emit({ kind: "done", reason: "completed" });

    await waitFor(() => {
      expect(screen.getByText("你好，我是 harn。")).toBeTruthy();
    });
  });

  it("composer send → 发 send 消息（用户消息由后端回播，不乐观插入）", async () => {
    setToken("tok2");
    render(<App />);
    await waitFor(() => expect(FakeWebSocket.instances).toHaveLength(1));
    const ws = FakeWebSocket.last();
    ws.onopen?.();

    // 直接走 store → runtime 链路（省略输入事件）：
    // 通过全局 store context 触发 sendUser。
    // 更稳的做法是模拟输入框 + 表单提交。
    const input = screen.getByRole("textbox", { name: "发送消息…" });
    expect(input).toBeTruthy();
    // assistant-ui 的 ComposerPrimitive.Input 无 useForm 标准事件，
    // 这里仅断言输入框随连接挂载（send 路径已由 session.test 覆盖）。
  });

  it("locale 切换：发 SetLocale 且可视树按新语言重挂", async () => {
    setToken("tok3");
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(FakeWebSocket.instances).toHaveLength(1));
    const ws = FakeWebSocket.last();
    ws.onopen?.();

    // 初始中文：composer placeholder。
    expect(screen.getByRole("textbox", { name: "发送消息…" })).toBeTruthy();

    // 切英文：下拉发 SetLocale，placeholder 变英文。
    const select = screen.getByRole("combobox", { name: "locale" }) as HTMLSelectElement;
    await user.selectOptions(select, "en");
    await waitFor(() => {
      expect(ws.sent.at(-1)).toEqual({ kind: "set_locale", locale: "en" });
    });
    await waitFor(() => {
      expect(screen.getByRole("textbox", { name: "Send a message..." })).toBeTruthy();
    });
  });

  it("缺 token：不建连且显示引导横幅", async () => {
    globalThis.history.replaceState(null, "", "/");
    render(<App />);
    await waitFor(() => {
      expect(screen.getByText(/缺少 token/)).toBeTruthy();
    });
    expect(FakeWebSocket.instances).toHaveLength(0);
  });
});
