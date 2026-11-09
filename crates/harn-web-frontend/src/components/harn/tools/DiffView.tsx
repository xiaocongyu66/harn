// marked diff 渲染：harn 的 render_diff_marked 输出解析为 +/-/ctx 着色行。

import { parseMarkedDiff } from "@/lib/marked-diff";
import { cn } from "@/lib/utils";

export function DiffView({ text }: { text: string }) {
  const hunks = parseMarkedDiff(text);
  if (hunks.length === 0) return null;

  return (
    <div className="my-2 overflow-hidden rounded-md border">
      {hunks.map((hunk, index) => (
        <div key={`${hunk.header}-${index}`}>
          <div className="bg-muted/60 text-muted-foreground px-2.5 py-1 font-mono text-[11px]">
            {hunk.header}
          </div>
          {hunk.lines.map((line, lineIndex) => (
            <div
              key={`${line.line}-${lineIndex}`}
              className={cn(
                "flex gap-2 px-2.5 py-0.5 font-mono text-[12px]",
                line.kind === "add" && "bg-emerald-500/15",
                line.kind === "del" && "bg-red-500/15",
              )}
            >
              <span className="text-muted-foreground w-8 shrink-0 select-none text-right">
                {line.line ?? ""}
              </span>
              <span
                className={cn(
                  "shrink-0",
                  line.kind === "add" && "text-emerald-600",
                  line.kind === "del" && "text-red-600",
                  line.kind === "ctx" && "text-muted-foreground",
                )}
              >
                {line.kind === "add" ? "+" : line.kind === "del" ? "-" : " "}
              </span>
              <span className="whitespace-pre-wrap break-all">{line.text}</span>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

/** 结果文本是否是 marked diff（决定 Edit 卡片走 diff 还是旧/新文本）。 */
export { looksLikeMarkedDiff } from "@/lib/marked-diff";
