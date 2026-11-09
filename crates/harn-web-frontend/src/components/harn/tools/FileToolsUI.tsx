// 文件类工具卡：Read（预览）/ Write（限高全文）/ Edit（marked diff）。

import { makeAssistantToolUI } from "@assistant-ui/react";
import type { ReactNode } from "react";

import { ArgsGrid, OutputPre, ToolShell } from "./shared";
import { DiffView, looksLikeMarkedDiff } from "./DiffView";

const fileTitle = (path?: string, label = ""): ReactNode => (
  <span className="font-mono text-[12px]">
    {label && <span className="text-muted-foreground mr-1">{label}</span>}
    {path ?? "(未知路径)"}
  </span>
);

interface ReadArgs {
  file_path?: string;
  offset?: number;
  limit?: number;
}

export const ReadToolUI = makeAssistantToolUI<ReadArgs, string>({
  toolName: "Read",
  render: ({ args, result, isError, status, approval, respondToApproval }) => {
    const extra: Array<[string, ReactNode]> = [];
    if (args?.offset !== undefined) extra.push(["offset", String(args.offset)]);
    if (args?.limit !== undefined) extra.push(["limit", String(args.limit)]);
    return (
      <ToolShell
        status={status}
        approval={approval}
        respondToApproval={respondToApproval}
        title={fileTitle(args?.file_path)}
        subtitle="Read"
      >
        <ArgsGrid entries={extra} />
        {result !== undefined && <OutputPre text={String(result)} isError={isError} />}
      </ToolShell>
    );
  },
});

interface WriteArgs {
  file_path?: string;
  content?: string;
}

export const WriteToolUI = makeAssistantToolUI<WriteArgs, string>({
  toolName: "Write",
  render: ({ args, result, isError, status, approval, respondToApproval }) => (
    <ToolShell
      status={status}
      approval={approval}
      respondToApproval={respondToApproval}
      title={fileTitle(args?.file_path)}
      subtitle="Write"
    >
      {args?.content !== undefined && (
        <OutputPre text={args.content} maxHeight={240} copyable={true} />
      )}
      {result !== undefined && <OutputPre text={String(result)} isError={isError} />}
    </ToolShell>
  ),
});

interface EditArgs {
  file_path?: string;
  old_string?: string;
  new_string?: string;
  replace_all?: boolean;
}

export const EditToolUI = makeAssistantToolUI<EditArgs, string>({
  toolName: "Edit",
  render: ({ args, result, isError, status, approval, respondToApproval }) => {
    const output = typeof result === "string" ? result : "";
    const diffMode = looksLikeMarkedDiff(output);
    return (
      <ToolShell
        status={status}
        approval={approval}
        respondToApproval={respondToApproval}
        title={fileTitle(args?.file_path, args?.replace_all ? "全部替换" : undefined)}
        subtitle="Edit"
      >
        <ArgsGrid
          entries={
            args?.replace_all ? [["replace_all", "true"]] : []
          }
        />
        {diffMode ? (
          <DiffView text={output} />
        ) : (
          <>
            {args?.old_string !== undefined && (
              <OutputPre label="旧" text={args.old_string} maxHeight={180} />
            )}
            {args?.new_string !== undefined && (
              <OutputPre label="新" text={args.new_string} maxHeight={180} />
            )}
          </>
        )}
        {result !== undefined && !diffMode && (
          <OutputPre text={output} isError={isError} />
        )}
      </ToolShell>
    );
  },
});
