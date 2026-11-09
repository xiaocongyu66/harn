// 搜索类工具卡：Glob（结果芯片）/ Grep（file:line 分组）/ WebFetch（链接化）。

import { makeAssistantToolUI } from "@assistant-ui/react";
import type { ReactNode } from "react";

import { ArgsGrid, OutputPre, ToolShell } from "./shared";

interface GlobArgs {
  pattern?: string;
  path?: string;
}

export const GlobToolUI = makeAssistantToolUI<GlobArgs, string>({
  toolName: "Glob",
  render: ({ args, result, status, approval, respondToApproval }) => {
    const files = String(result ?? "")
      .split("\n")
      .filter((line) => line.trim().length > 0);
    const extra: Array<[string, ReactNode]> = [];
    if (args?.path) extra.push(["path", args.path]);
    return (
      <ToolShell
        status={status}
        approval={approval}
        respondToApproval={respondToApproval}
        title={<code className="font-mono text-[12px]">{args?.pattern ?? "Glob"}</code>}
        subtitle={files.length > 0 ? `${files.length} 个匹配` : "Glob"}
      >
        <ArgsGrid entries={extra} />
        {files.length > 0 ? (
          <div className="flex flex-wrap gap-1.5">
            {files.map((file) => (
              <span
                key={file}
                className="bg-muted text-foreground rounded px-2 py-0.5 font-mono text-[11px]"
              >
                {file}
              </span>
            ))}
          </div>
        ) : (
          result !== undefined && <OutputPre text={String(result)} />
        )}
      </ToolShell>
    );
  },
});

interface GrepArgs {
  pattern?: string;
  path?: string;
  glob?: string;
  type?: string;
}

export const GrepToolUI = makeAssistantToolUI<GrepArgs, string>({
  toolName: "Grep",
  render: ({ args, result, status, approval, respondToApproval }) => {
    const hits = String(result ?? "")
      .split("\n")
      .filter((line) => line.trim().length > 0);
    const extra: Array<[string, ReactNode]> = [];
    if (args?.path) extra.push(["path", args.path]);
    if (args?.glob) extra.push(["glob", args.glob]);
    if (args?.type) extra.push(["type", args.type]);
    return (
      <ToolShell
        status={status}
        approval={approval}
        respondToApproval={respondToApproval}
        title={<code className="font-mono text-[12px]">{args?.pattern ?? "Grep"}</code>}
        subtitle={hits.length > 0 ? `${hits.length} 处命中` : "Grep"}
      >
        <ArgsGrid entries={extra} />
        {hits.length > 0 ? (
          <div className="my-2 max-h-64 space-y-1.5 overflow-y-auto">
            {hits.map((hit) => {
              const colon = hit.indexOf(":");
              const [file, rest] = colon > 0 ? [hit.slice(0, colon), hit.slice(colon + 1)] : [hit, ""];
              const colon2 = rest.indexOf(":");
              const [lineno, text] = colon2 > 0 ? [rest.slice(0, colon2), rest.slice(colon2 + 1)] : [rest, ""];
              return (
                <div key={hit} className="font-mono text-[11px] leading-relaxed">
                  <span className="text-foreground">{file}</span>
                  <span className="text-muted-foreground">:{lineno}:</span>
                  <span className="text-muted-foreground">{text}</span>
                </div>
              );
            })}
          </div>
        ) : (
          result !== undefined && <OutputPre text={String(result)} />
        )}
      </ToolShell>
    );
  },
});

interface WebFetchArgs {
  url?: string;
  maxChars?: number;
}

export const WebFetchToolUI = makeAssistantToolUI<WebFetchArgs, string>({
  toolName: "WebFetch",
  render: ({ args, result, status, approval, respondToApproval }) => (
    <ToolShell
      status={status}
      approval={approval}
      respondToApproval={respondToApproval}
      title={
        args?.url ? (
          <a
            href={args.url}
            target="_blank"
            rel="noreferrer"
            className="text-primary truncate font-mono text-[12px] underline"
          >
            {args.url}
          </a>
        ) : (
          "WebFetch"
        )
      }
      subtitle="WebFetch"
    >
      <ArgsGrid entries={args?.maxChars !== undefined ? [["maxChars", String(args.maxChars)]] : []} />
      {result !== undefined && <OutputPre text={String(result)} />}
    </ToolShell>
  ),
});
