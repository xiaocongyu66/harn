// 工具卡公共件：外壳（折叠/状态）、审批按钮、参数表、折叠输出。
//
// approval 渲染约定：part.approval.approved === undefined 且无 resolution
// 时为待决 → 渲染允许一次/总是允许/拒绝三按钮（与 harn PermissionChoice
// 一一对应），respondToApproval 经 runtime 回到 onRespondToToolApproval。

import {
  CheckIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  LoaderIcon,
  XIcon,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import { t } from "@/i18n";

import type {
  MessagePartState,
  ToolApprovalResponse,
  ToolCallMessagePart,
} from "@assistant-ui/react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** 卡片收到的库级 props 子集（render 函数形参）。 */
export interface ToolCardProps {
  /** render 形参里的 status：消息级与 part 级状态的联合。 */
  status: MessagePartState["status"];
  approval?: ToolCallMessagePart["approval"];
  respondToApproval: (response: ToolApprovalResponse) => Promise<void>;
}

export function ToolShell({
  title,
  subtitle,
  status,
  approval,
  respondToApproval,
  defaultOpen = false,
  children,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  status: ToolCardProps["status"];
  approval?: ToolCardProps["approval"];
  respondToApproval?: ToolCardProps["respondToApproval"];
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  // approval 存在但无回传函数时审批按钮退化为纯展示。
  respondToApproval ??= async () => {};
  const [open, setOpen] = useState(defaultOpen);

  return (
    <div className="bg-card text-card-foreground my-2 overflow-hidden rounded-lg border">
      <div className="flex items-center gap-2 px-3 py-2">
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          className="text-muted-foreground hover:text-foreground shrink-0"
          aria-label={open ? "折叠" : "展开"}
        >
          {open ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
        </button>
        <StatusIcon status={status} />
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{title}</div>
          {subtitle && (
            <div className="text-muted-foreground truncate text-xs">{subtitle}</div>
          )}
        </div>
      </div>

      {approval && <ApprovalBanner approval={approval} respondToApproval={respondToApproval} />}

      {open && <div className="border-t px-3 py-2">{children}</div>}
    </div>
  );
}

function StatusIcon({ status }: { status: ToolCardProps["status"] }) {
  if (status.type === "running") {
    return <LoaderIcon className="text-muted-foreground size-4 animate-spin" />;
  }
  if (status.type === "incomplete") {
    return <XIcon className="text-destructive size-4" />;
  }
  return <CheckIcon className="text-muted-foreground size-4" />;
}

function ApprovalBanner({
  approval,
  respondToApproval,
}: {
  approval: NonNullable<ToolCardProps["approval"]>;
  respondToApproval: ToolCardProps["respondToApproval"];
}) {
  const settled = approval.approved !== undefined;

  if (settled) {
    return (
      <div className="text-muted-foreground border-t px-3 py-1.5 text-xs">
        {approval.approved ? t("approval.allowed") : t("approval.refused")}
      </div>
    );
  }

  const options = approval.options ?? [];
  return (
    <div className="border-t px-3 py-2">
      {approval.prompt && (
        <p className="text-destructive mb-2 text-xs">{approval.prompt}</p>
      )}
      <div className="flex flex-wrap gap-2">
        {options.map((option) => (
          <Button
            key={option.id}
            size="sm"
            variant={option.kind === "reject-once" ? "destructive" : "default"}
            onClick={() => respondToApproval({ optionId: option.id })}
          >
            {option.label ?? option.id}
          </Button>
        ))}
      </div>
    </div>
  );
}

/** 字段表：次要入参以键值对陈列。 */
export function ArgsGrid({ entries }: { entries: [string, ReactNode][] }) {
  if (entries.length === 0) return null;
  return (
    <dl className="text-muted-foreground mb-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
      {entries.map(([key, value]) => (
        <div key={key} className="contents">
          <dt className="font-medium">{key}</dt>
          <dd className="text-foreground truncate">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

/** 折叠的等宽输出（工具结果/文件内容）。 */
export function OutputPre({
  text,
  label,
  maxHeight = 400,
  isError = false,
  copyable = true,
}: {
  text: string;
  label?: string;
  maxHeight?: number;
  isError?: boolean;
  copyable?: boolean;
}) {
  const [copied, setCopied] = useState(false);

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // 剪贴板不可用（无 HTTPS/权限）时静默。
    }
  };

  return (
    <div className="relative my-2">
      {label && (
        <div className="text-muted-foreground mb-1 text-xs">{label}</div>
      )}
      <pre
        className={cn(
          "bg-muted/40 overflow-x-auto rounded-md p-2.5 text-[12px] leading-relaxed",
          isError && "text-destructive",
        )}
        style={{ maxHeight, overflowY: "auto" }}
      >
        {text || t("tool.emptyOutput")}
      </pre>
      {copyable && (
        <Button
          size="sm"
          variant="ghost"
          className="absolute right-1 top-1 opacity-60 hover:opacity-100"
          onClick={onCopy}
        >
          {copied ? t("thread.copied") : t("thread.copy")}
        </Button>
      )}
    </div>
  );
}
