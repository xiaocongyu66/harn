// harn 会话归约器：把 ServerMsg 流归约为 assistant-ui 的 ThreadMessageLike[]。
//
// 核心约定（见 plans/splendid-hugging-lagoon.md §20）：
// - 重连即重建：每次连接重置 store，先吃快照再吃增量（后端不重播已消费事件）；
// - 一个 (turn,agentId) 一条助手消息：delta/thinking/tool-call 都作 part 追加，
//   thread.aui 的 groupPartByType 负责分组折叠（等价思考链视图）；
// - 审批到达时必有同名未出结果 tool-call part（后端 ToolStart 先于权限请求），
//   挂 approval.options 并**显式**把消息 status 设为 requires-action——
//   auto-status 里 isLast&&isRunning 优先级更高，不显式覆盖会隐藏审批按钮；
// - 用户 Deny 时后端只发 PermissionDenied 不发 ToolResult，必须自补合成
//   result，否则该 part 永远 running。

import type { ThreadMessageLike } from "@assistant-ui/react";

import { IdGenerator } from "./id";
import type {
  ClientMsg,
  EntryDto,
  ModalDto,
  QuestionOptionDto,
  ServerMsg,
  StatusDto,
} from "./types";

/** 助手消息的 part 数组形态（content 也允许纯字符串：仅 user 消息）。 */
type MessageParts = Exclude<ThreadMessageLike["content"], string>;
type AnyPart = MessageParts[number];

/** 审批选项（与 harn 的三个 PermissionChoice 一一对应）。 */
export interface HarnApprovalOption {
  id: string;
  kind: "allow-once" | "allow-always" | "reject-once";
  label: string;
}

export interface QuestionState {
  question: string;
  header: string;
  options: QuestionOptionDto[];
  multiSelect: boolean;
}

export interface SessionState {
  messages: ThreadMessageLike[];
  question: QuestionState | null;
  statusLines: string[];
  running: boolean;
  /** 当前 pending 的权限审批（用于回包映射）。 */
  pendingApproval: string | null;
  /** 后端 Status（协议兼容接收，当前两个桥都不发）。 */
  status: StatusDto | null;
}

interface ToolPart {
  type: "tool-call";
  toolCallId: string;
  toolName: string;
  args?: unknown;
  argsText?: string;
  result?: unknown;
  isError?: boolean;
  approval?: {
    id: string;
    prompt?: string;
    options: HarnApprovalOption[];
    approved?: boolean;
  };
}

/** 配对队列项：(agentId,name) → toolCallId。 */
interface PendingTool {
  messageId: string;
  toolCallId: string;
  agentId: string;
  name: string;
}

const APPROVAL_OPTIONS: HarnApprovalOption[] = [
  { id: "allow-once", kind: "allow-once", label: "允许一次" },
  { id: "allow-always", kind: "allow-always", label: "总是允许" },
  { id: "reject-once", kind: "reject-once", label: "拒绝" },
];

export class SessionStore {
  private ids = new IdGenerator();
  private turn = 0;
  /** agentId → 当前 turn 的在途助手消息 id。 */
  private inflight = new Map<string, string>();
  private pendingTools: PendingTool[] = [];
  private state: SessionState = {
    messages: [],
    question: null,
    statusLines: [],
    running: false,
    pendingApproval: null,
    status: null,
  };
  private listeners = new Set<() => void>();
  private sender: (msg: ClientMsg) => void;

  constructor(sender: (msg: ClientMsg) => void) {
    this.sender = sender;
  }

  // ---- React 外部存储接口（useSyncExternalStore 友好）----

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getSnapshot = (): SessionState => this.state;

  // ---- 重连：整体重建 ----

  reset() {
    this.ids = new IdGenerator();
    this.turn = 0;
    this.inflight.clear();
    this.pendingTools = [];
    this.state = {
      messages: [],
      question: null,
      statusLines: [],
      running: false,
      pendingApproval: null,
      status: null,
    };
    this.emit();
  }

  // ---- ServerMsg 归约 ----

  dispatch(message: ServerMsg) {
    switch (message.kind) {
      case "entry":
        this.reduceEntry(message.entry);
        break;
      case "delta":
        this.appendDelta(message.agent_id, message.text);
        break;
      case "modal":
        this.reduceModal(message.modal);
        break;
      case "status":
        this.state = { ...this.state, status: message.status };
        break;
      case "done":
        this.finishTurn(message.reason);
        break;
      case "error":
        // 协议层错误 → 状态行（agent 错误走 Entry{Error}）。
        this.state = {
          ...this.state,
          statusLines: [...this.state.statusLines, message.message],
        };
        break;
    }
    this.emit();
  }

  // ---- 前端动作 → ClientMsg ----

  sendUser(text: string) {
    // turn 先于发送递增：保证后续 Delta/Tool 归入新 turn；用户消息由后端
    // 回播 Entry{User} 插入，前端不乐观插入（否则重复）。
    this.turn += 1;
    this.inflight.clear();
    this.pendingTools = [];
    this.state = { ...this.state, running: true, pendingApproval: null };
    this.sender({ kind: "send", text });
    this.emit();
  }

  requestAbort() {
    this.sender({ kind: "abort" });
  }

  /** runtime 回灌的消息列表（如 cancel 时的末条删除）；直接接受。 */
  replaceMessages(messages: readonly ThreadMessageLike[]) {
    this.state = { ...this.state, messages: [...messages] };
    this.emit();
  }

  respondApproval(approvalId: string, choice: "allow_once" | "allow_always" | "deny") {
    this.sender({ kind: "choose", reply: { kind: "permission", choice } });
    // 乐观更新：置位 approved；Deny 补合成 result（后端不发 ToolResult）。
    const messages = this.state.messages.map((message) =>
      this.applyApprovalResponse(message, approvalId, choice),
    );
    this.state = { ...this.state, messages, pendingApproval: null };
    this.emit();
  }

  respondQuestion(answers: string[] | null) {
    this.sender({ kind: "choose", reply: { kind: "question", answers } });
    this.state = { ...this.state, question: null };
    this.emit();
  }

  setMode(mode: string) {
    this.sender({ kind: "set_mode", mode });
  }

  setLocale(locale: string) {
    this.sender({ kind: "set_locale", locale });
  }

  // ---- 内部归约 ----

  private emit() {
    for (const listener of this.listeners) listener();
  }

  private reduceEntry(entry: EntryDto) {
    switch (entry.kind) {
      case "user":
        this.state.messages.push({
          role: "user",
          id: this.ids.user(),
          content: entry.text,
          ...(entry.images > 0 ? { metadata: { custom: { images: entry.images } } } : {}),
        });
        break;
      case "assistant":
        this.setAuthoritativeText(entry.agent_id, entry.text);
        break;
      case "thinking":
        this.appendPart(entry.agent_id, { type: "reasoning", text: entry.text });
        break;
      case "tool": {
        const messageId = this.ensureInflight(entry.agent_id);
        const toolCallId = this.ids.toolCall(this.turn, entry.agent_id, entry.name);
        const part: ToolPart = {
          type: "tool-call",
          toolCallId,
          toolName: entry.name,
          args: entry.input,
          argsText: JSON.stringify(entry.input, null, 2),
        };
        this.pendingTools.push({
          messageId,
          toolCallId,
          agentId: entry.agent_id,
          name: entry.name,
        });
        this.state.messages = this.state.messages.map((message) =>
          message.id === messageId
            ? ({ ...message, content: [...asParts(message.content), part] } as ThreadMessageLike)
            : message,
        );
        break;
      }
      case "tool_result": {
        this.applyToolResult(entry.agent_id, entry.name, entry.output, entry.is_error);
        break;
      }
      case "status":
        this.state.statusLines = [...this.state.statusLines, entry.text];
        break;
      case "error":
        this.state.messages.push({
          role: "assistant",
          id: `e-${this.ids.user()}`,
          content: entry.text,
          status: { type: "incomplete" as const, reason: "error" as const, error: entry.text },
        } as ThreadMessageLike);
        break;
    }
    this.state = { ...this.state, messages: [...this.state.messages] };
  }

  private appendDelta(agentId: string, text: string) {
    const messageId = this.ensureInflight(agentId);
    this.state.messages = this.state.messages.map((message) => {
      if (message.id !== messageId) return message;
      const parts = asParts(message.content);
      const last: AnyPart | undefined = parts.at(-1);
      if (last && last.type === "text") {
        return {
          ...message,
          content: [...parts.slice(0, -1), { type: "text", text: last.text + text }],
        } as ThreadMessageLike;
      }
      return { ...message, content: [...parts, { type: "text", text }] } as ThreadMessageLike;
    });
  }

  private setAuthoritativeText(agentId: string, text: string) {
    const messageId = this.ensureInflight(agentId);
    this.state.messages = this.state.messages.map((message) => {
      const parts = asParts(message.content);
      if (message.id === messageId && parts.some((part) => part.type === "text")) {
        // 纠偏流式累积：首个 text part 换成权威全文，其余 text part 丢掉。
        const rest = parts.filter((part) => part.type !== "text");
        return { ...message, content: [{ type: "text", text }, ...rest] } as ThreadMessageLike;
      }
      return message;
    });
  }

  private appendPart(agentId: string, part: AnyPart) {
    const messageId = this.ensureInflight(agentId);
    this.state.messages = this.state.messages.map((message) =>
      message.id === messageId
        ? ({ ...message, content: [...asParts(message.content), part] } as ThreadMessageLike)
        : message,
    );
  }

  /** 取/建当前 turn 的在途助手消息，返回其 id。 */
  private ensureInflight(agentId: string): string {
    const existing = this.inflight.get(agentId);
    if (existing) return existing;
    const id = this.ids.assistant(this.turn, agentId);
    this.inflight.set(agentId, id);
    this.state.messages.push({ role: "assistant", id, content: [] } as ThreadMessageLike);
    return id;
  }

  private applyToolResult(
    agentId: string,
    name: string,
    output: string,
    isError: boolean,
  ) {
    const index = this.pendingTools.findIndex(
      (pending) => pending.agentId === agentId && pending.name === name,
    );
    if (index < 0) return;
    const target = this.pendingTools[index];
    this.pendingTools.splice(index, 1);
    this.state.messages = this.state.messages.map((message) =>
      message.id === target.messageId
        ? {
            ...message,
            content: asParts(message.content).map((part) =>
              part.type === "tool-call" && part.toolCallId === target.toolCallId
                ? { ...part, result: output, isError }
                : part,
            ),
          }
        : message,
    );
  }

  private reduceModal(modal: ModalDto) {
    if (modal.kind === "question") {
      this.state = {
        ...this.state,
        question: {
          question: modal.question,
          header: modal.header,
          options: modal.options,
          multiSelect: modal.multi_select,
        },
      };
      return;
    }
    // 权限审批：挂到当前 turn 最新未出结果同名 tool part（后端时序保证存在）。
    const target = [...this.pendingTools]
      .reverse()
      .find((pending) => pending.name === modal.tool);
    if (!target) {
      this.state = {
        ...this.state,
        statusLines: [...this.state.statusLines, `${modal.tool}: ${modal.reason}`],
      };
      return;
    }
    const approvalId = this.ids.approval(target.toolCallId);
    this.state.pendingApproval = approvalId;
    this.state.messages = this.state.messages.map((message) =>
      message.id === target.messageId
        ? ({
            ...message,
            // 显式 requires-action：否则被 auto-status 的 running 压住，
            // 审批按钮不出现。
            status: { type: "requires-action" as const, reason: "tool-calls" as const },
            content: asParts(message.content).map((part) =>
              part.type === "tool-call" && part.toolCallId === target.toolCallId
                ? {
                    ...part,
                    approval: {
                      id: approvalId,
                      prompt: modal.reason,
                      options: APPROVAL_OPTIONS,
                    },
                  }
                : part,
            ),
          } as ThreadMessageLike)
        : message,
    );
  }

  private applyApprovalResponse(
    message: ThreadMessageLike,
    approvalId: string,
    choice: "allow_once" | "allow_always" | "deny",
  ): ThreadMessageLike {
    const parts = asParts(message.content);
    if (!parts.some((part) => part.type === "tool-call" && part.approval?.id === approvalId)) {
      return message;
    }
    const deny = choice === "deny";
    const allow = !deny;
    return {
      ...message,
      content: parts.map((part) => {
        if (part.type !== "tool-call" || part.approval?.id !== approvalId) return part;
        return {
          ...part,
          approval: { ...part.approval, approved: allow },
          // Deny 时后端不发 ToolResult：补合成 result，避免 part 永远 running。
          ...(deny ? { result: "已由用户拒绝", isError: true } : {}),
        };
      }),
      // 审批落定后 status 回归 auto（后续 ToolResult/Done 接管）。
      status: undefined,
    };
  }

  private finishTurn(reason: string) {
    const aborted = reason.includes("abort") || reason.includes("cancel");
    this.inflight.clear();
    this.pendingTools = [];
    const messages = this.state.messages.map((message, index, array) => {
      if (index !== array.length - 1) return message;
      if (aborted && message.status === undefined) {
        return { ...message, status: { type: "incomplete" as const, reason: "cancelled" as const } };
      }
      // 若消息仍带显式 requires-action（如等待审批时被 Done）则交还 auto。
      return message;
    });
    this.state = { ...this.state, messages, running: false, pendingApproval: null };
  }
}

/** content 可能是纯字符串（仅 user 消息）；助手消息统一按 parts 处理。 */
function asParts(content: ThreadMessageLike["content"]): MessageParts {
  return (typeof content === "string" ? [] : [...content]) as MessageParts;
}
