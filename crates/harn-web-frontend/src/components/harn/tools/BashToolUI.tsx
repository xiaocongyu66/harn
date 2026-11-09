// Bash：命令 + 说明 + 超时/工作目录明细 + 折叠输出（错误红字）。

import { makeAssistantToolUI } from "@assistant-ui/react";
import type { ReactNode } from "react";

import { ArgsGrid, OutputPre, ToolShell } from "./shared";

interface BashArgs {
  command?: string;
  description?: string;
  timeout?: number | string;
  cwd?: string;
}

export const BashToolUI = makeAssistantToolUI<BashArgs, string>({
  toolName: "Bash",
  render: ({ args, result, isError, status, approval, respondToApproval }) => {
    const extra: Array<[string, ReactNode]> = [];
    if (args?.timeout !== undefined) extra.push(["timeout", String(args.timeout)]);
    if (args?.cwd) extra.push(["cwd", args.cwd]);

    return (
      <ToolShell
        status={status}
        approval={approval}
        respondToApproval={respondToApproval}
        title={<code className="font-mono text-[12px]">{args?.command ?? "(空命令)"}</code>}
        subtitle={args?.description ?? "Bash"}
        defaultOpen={true}
      >
        <ArgsGrid entries={extra} />
        {result !== undefined && (
          <OutputPre text={typeof result === "string" ? result : String(result)} isError={isError} />
        )}
      </ToolShell>
    );
  },
});
