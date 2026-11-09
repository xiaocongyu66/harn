// harn WebSocket 客户端：模块级单例 + 引用计数订阅 + 指数退避重连。
//
// 单例的原因：React StrictMode/重渲染会让 effect 多跑，直接在 effect 里建连
// 会产生双连。effect 只做 subscribe/unsubscribe（计数），真正的连接由单例
// 持有；\"断开即重连\"由内部看门狗负责（后端 daemon reload / 网络抖动时自愈）。

import { isServerMsg, type ServerMsg } from "./types";

export type ServerMsgListener = (message: ServerMsg) => void;
export type ConnectionStateListener = (state: "connecting" | "open" | "closed") => void;

const MAX_RETRY_DELAY = 8_000;

export class HarnSocket {
  private url: string | null = null;
  private ws: WebSocket | null = null;
  private listeners = new Set<ServerMsgListener>();
  private stateListeners = new Set<ConnectionStateListener>();
  private retry = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  state: "connecting" | "open" | "closed" = "closed";

  /** 从 location.search 取 token，拼相对 /ws?token=…；无 token 返回 null。 */
  static urlFromLocation(): string | null {
    const search = globalThis.location?.search ?? "";
    const token = search
      .split("&")
      .map((pair) => pair.replace(/^\?/, ""))
      .find((pair) => pair.startsWith("token="))
      ?.slice("token=".length);
    return token ? `/ws?token=${encodeURIComponent(token)}` : null;
  }

  /** 订阅消息流；返回取消订阅函数。 */
  onMessage(listener: ServerMsgListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** 订阅连接状态变化。 */
  onState(listener: ConnectionStateListener): () => void {
    this.stateListeners.add(listener);
    listener(this.state);
    return () => this.stateListeners.delete(listener);
  }

  connect(url: string) {
    this.url = url;
    this.retry = 0;
    this.open();
  }

  send(payload: unknown) {
    const json = JSON.stringify(payload);
    this.ws?.send(json);
  }

  /** 关闭且不再自动重连（组件卸载/换会话时）。 */
  shutdown() {
    this.url = null;
    this.clearRetryTimer();
    this.ws?.close();
    this.ws = null;
    this.setState("closed");
  }

  private open() {
    if (!this.url) return;
    this.setState("connecting");
    try {
      this.ws = new WebSocket(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws.onopen = () => {
      this.retry = 0;
      this.setState("open");
    };
    this.ws.onmessage = (event) => {
      const raw = typeof event.data === "string" ? event.data : null;
      if (!raw) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch {
        return; // 非 JSON 帧忽略（保命：协议演进不崩）。
      }
      if (!isServerMsg(parsed)) return;
      for (const listener of this.listeners) listener(parsed);
    };
    this.ws.onclose = () => {
      this.ws = null;
      this.setState("closed");
      this.scheduleReconnect();
    };
    this.ws.onerror = () => {
      // 让 onclose 兜底重连；error 单独不处理。
    };
  }

  private scheduleReconnect() {
    if (!this.url || this.retryTimer) return;
    const delay = Math.min(1_000 * 2 ** this.retry, MAX_RETRY_DELAY);
    this.retry += 1;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.open();
    }, delay);
  }

  private clearRetryTimer() {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
  }

  private setState(state: "connecting" | "open" | "closed") {
    if (this.state === state) return;
    this.state = state;
    for (const listener of this.stateListeners) listener(state);
  }
}

export const harnSocket = new HarnSocket();
