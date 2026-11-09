// 解析 harn `render_diff_marked` 的纯文本 diff（无 ANSI，harn-tools/src/diff.rs）。
//
// 固定格式：
//   @@ -oldStart,oldLines +newStart,newLines @@
//   +[ 12] added line
//   -[ 11] removed line
//    [ 10] context line
//
// 前端只做渲染归约：harn 已经把 unified 组装好，不需要 patch/diff 库。

export interface DiffLine {
  kind: "add" | "del" | "ctx";
  line: number | null;
  text: string;
}

export interface DiffHunk {
  header: string;
  lines: DiffLine[];
}

const LINE_RE = /^([+\- ])\[\s*(\d+)?\s*\](.*)$/;
const HEADER_RE = /^@@/;

export function parseMarkedDiff(output: string): DiffHunk[] {
  const hunks: DiffHunk[] = [];
  let current: DiffHunk | null = null;

  for (const raw of output.split("\n")) {
    if (!raw) continue;
    if (HEADER_RE.test(raw)) {
      current = { header: raw, lines: [] };
      hunks.push(current);
      continue;
    }
    const match = LINE_RE.exec(raw);
    if (!match || !current) continue;
    const [, sign, line, text] = match;
    current.lines.push({
      kind: sign === "+" ? "add" : sign === "-" ? "del" : "ctx",
      line: line ? Number(line) : null,
      text: text ?? "",
    });
  }
  return hunks;
}

/** 全文本是否是 marked diff（用于 Edit 结果的渲染分支判定）。 */
export function looksLikeMarkedDiff(output: string): boolean {
  return /^@@.*@@/m.test(output) && LINE_RE.test(output);
}
