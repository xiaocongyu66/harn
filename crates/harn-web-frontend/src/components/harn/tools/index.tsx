// 工具卡注册表：渲染即注册（makeAssistantToolUI）。
// 未注册工具自动走 ToolFallback（含审批/收据/错误态），不裸 JSON。

import { AgentToolUI } from "./MiscToolsUI";
import { TeamToolUI, TodoWriteToolUI, EnterPlanModeToolUI, ExitPlanModeToolUI } from "./MiscToolsUI";
import { GlobToolUI, GrepToolUI, WebFetchToolUI } from "./SearchToolsUI";
import { EditToolUI, ReadToolUI, WriteToolUI } from "./FileToolsUI";
import { BashToolUI } from "./BashToolUI";

export function ToolRegistry() {
  return (
    <>
      <BashToolUI />
      <ReadToolUI />
      <WriteToolUI />
      <EditToolUI />
      <GlobToolUI />
      <GrepToolUI />
      <WebFetchToolUI />
      <TodoWriteToolUI />
      <EnterPlanModeToolUI />
      <ExitPlanModeToolUI />
      <AgentToolUI />
      <TeamToolUI />
    </>
  );
}
