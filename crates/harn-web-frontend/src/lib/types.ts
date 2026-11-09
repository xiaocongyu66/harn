// harn 协议 TS 镜像（与 crates/harn-web-protocol/src/lib.rs 的 serde
// tag="kind" + rename_all="snake_case" 一一对应）。

export type ClientMsg =
  | { kind: "send"; text: string }
  | { kind: "abort" }
  | { kind: "choose"; reply: ModalReply }
  | { kind: "set_mode"; mode: string }
  | { kind: "set_locale"; locale: string };

export type ModalReply =
  | { kind: "permission"; choice: "allow_once" | "allow_always" | "deny" }
  | { kind: "question"; answers: string[] | null };

export type ServerMsg =
  | { kind: "entry"; entry: EntryDto }
  | { kind: "delta"; agent_id: string; text: string }
  | { kind: "modal"; modal: ModalDto }
  | { kind: "status"; status: StatusDto }
  | { kind: "done"; reason: string }
  | { kind: "error"; message: string };

export type EntryDto =
  | { kind: "user"; text: string; images: number }
  | { kind: "assistant"; agent_id: string; text: string }
  | { kind: "thinking"; agent_id: string; text: string }
  | { kind: "tool"; agent_id: string; name: string; input: unknown; status: ToolStatusDto }
  | {
      kind: "tool_result";
      agent_id: string;
      name: string;
      output: string;
      is_error: boolean;
      ms: number;
    }
  | { kind: "status"; text: string }
  | { kind: "error"; text: string };

export type ToolStatusDto = "running" | "ok" | "error";

export type ModalDto =
  | { kind: "permission"; tool: string; target: string | null; reason: string }
  | {
      kind: "question";
      question: string;
      header: string;
      options: QuestionOptionDto[];
      multi_select: boolean;
    };

export interface QuestionOptionDto {
  label: string;
  description: string | null;
  preview: string | null;
}

export interface StatusDto {
  // 协议预留：mode/model/provider/token 计数/running 标志。
  [key: string]: unknown;
}

// 运行时判别：未知 kind 不致命（协议演进时前端不崩）。
export function isServerMsg(value: unknown): value is ServerMsg {
  if (typeof value !== "object" || value === null) return false;
  const kind = (value as { kind?: unknown }).kind;
  return (
    kind === "entry" ||
    kind === "delta" ||
    kind === "modal" ||
    kind === "status" ||
    kind === "done" ||
    kind === "error"
  );
}
