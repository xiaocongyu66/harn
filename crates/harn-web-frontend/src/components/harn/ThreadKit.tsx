// harn 的 Thread 装配：复用 elements/thread.aui 的 Thread，
// 仅把工具组改为默认展开（编码场景需要直视频繁的工具过程）。

import { type FC, type PropsWithChildren, useState } from "react";

import {
  Thread as BaseThread,
  type ThreadComponents,
} from "@/components/assistant-ui/elements/thread.aui";
import {
  ToolGroupContent,
  ToolGroupRoot,
  ToolGroupTrigger,
} from "@/components/assistant-ui/elements/tool-group.aui";
import type { ThreadGroupPart } from "@/components/assistant-ui/elements/thread.aui";

const OpenToolGroup: FC<PropsWithChildren<{ group: ThreadGroupPart }>> = ({
  group,
  children,
}) => {
  const [open, setOpen] = useState(true);

  return (
    <ToolGroupRoot
      variant="ghost"
      open={open || group.counts.requiresAction > 0}
      onOpenChange={setOpen}
    >
      <ToolGroupTrigger
        count={group.indices.length}
        active={group.status.type === "running"}
      />
      <ToolGroupContent>{children}</ToolGroupContent>
    </ToolGroupRoot>
  );
};

const components: ThreadComponents = {
  ToolGroup: OpenToolGroup,
};

export function HarnThread() {
  return <BaseThread components={components} autoFocus />;
}
