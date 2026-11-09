// 任务/计划/子代理类工具卡：TodoWrite、Enter/ExitPlanMode、Agent、Team。

import { makeAssistantToolUI } from "@assistant-ui/react";
import { CheckIcon, CircleIcon, LoaderIcon } from "lucide-react";
import type { ReactNode } from "react";

import { MarkdownText } from "@/components/assistant-ui/elements/markdown-text";
import { ArgsGrid, OutputPre, ToolShell } from "./shared";

interface Todo {
  status?: string;
  content?: string;
  activeForm?: string;
}

export const TodoWriteToolUI = makeAssistantToolUI<{ todos?: Todo[] }, string>({
  toolName: "TodoWrite",
  render: ({ args, status }) => {
    const todos = args?.todos ?? [];
    return (
      <ToolShell
        status={status}
        title="待办清单"
        subtitle={todos.length > 0 ? `${todos.length} 项` : undefined}
      >
        <ul className="my-1 space-y-1">
          {todos.map((todo, index) => (
            <li key={index} className="flex items-start gap-2 text-sm">
              <TodoIcon status={todo.status} />
              <span className={todo.status === "completed" ? "text-muted-foreground line-through" : ""}>
                {todo.activeForm || todo.content || "(无描述)"}
              </span>
            </li>
          ))}
        </ul>
      </ToolShell>
    );
  },
});

function TodoIcon({ status }: { status?: string }) {
  if (status === "in_progress") return <LoaderIcon className="mt-0.5 size-4 animate-spin" />;
  if (status === "completed") return <CheckIcon className="text-muted-foreground mt-0.5 size-4" />;
  return <CircleIcon className="text-muted-foreground mt-0.5 size-4" />;
}

export const EnterPlanModeToolUI = makeAssistantToolUI<{ reason?: string }, string>({
  toolName: "EnterPlanMode",
  render: ({ args, status }) => (
    <ToolShell status={status} title="进入计划模式" subtitle={args?.reason}>
      {args?.reason && <OutputPre label="reason" text={args.reason} />}
    </ToolShell>
  ),
});

export const ExitPlanModeToolUI = makeAssistantToolUI<{ plan?: string }, string>({
  toolName: "ExitPlanMode",
  render: ({ args, status }) => (
    <ToolShell status={status} title="计划已提交" subtitle="等待审批">
      {args?.plan && <MarkdownText text={args.plan} />}
    </ToolShell>
  ),
});

interface AgentArgs {
  description?: string;
  prompt?: string;
  subagent_type?: string;
  isolation?: string;
}

export const AgentToolUI = makeAssistantToolUI<AgentArgs, string>({
  toolName: "Agent",
  render: ({ args, result, status }) => {
    const extra: Array<[string, ReactNode]> = [];
    if (args?.subagent_type) extra.push(["类型", args.subagent_type]);
    if (args?.isolation) extra.push(["隔离", args.isolation]);
    return (
      <ToolShell
        status={status}
        title={args?.description ?? "子代理"}
        subtitle="Agent"
      >
        <ArgsGrid entries={extra} />
        {args?.prompt && <OutputPre label="任务" text={args.prompt} maxHeight={180} />}
        {result !== undefined && <OutputPre label="结果" text={String(result)} />}
      </ToolShell>
    );
  },
});

export const TeamToolUI = makeAssistantToolUI<AgentArgs, string>({
  toolName: "Team",
  render: ({ args, result, status }) => (
    <ToolShell status={status} title={args?.description ?? "代理团队"} subtitle="Team">
      {args?.prompt && <OutputPre label="目标" text={args.prompt} maxHeight={180} />}
      {result !== undefined && <OutputPre label="结果" text={String(result)} />}
    </ToolShell>
  ),
});
