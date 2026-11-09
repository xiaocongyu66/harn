// harn 运行时装配：WS ↔ SessionStore → assistant-ui 的 ExternalStoreRuntime。
//
// - 重连即重建：ws 的 open 事件触发 store.reset()（随后吃快照+增量）；
// - onNew 只发 send（用户消息等后端回播，不乐观插入）；
// - onCancel 发 abort；审批回包走 onRespondToToolApproval；
// - setMessages 供 runtime 回灌删除（如 cancel 时的末条删除）。

import {
  AssistantRuntimeProvider,
  useExternalStoreRuntime,
  type AppendMessage,
  type ThreadMessageLike,
} from "@assistant-ui/react";
import {
  createContext,
  useContext,
  useEffect,
  useState,
  useSyncExternalStore,
} from "react";

import type { ServerMsg } from "@/lib/types";
import { SessionStore } from "@/lib/session";
import { HarnSocket, harnSocket } from "@/lib/ws";

/** 会话 store 句柄（弹窗/状态栏等组件通过它发指令）。 */
export const HarnSessionContext = createContext<SessionStore | null>(null);

export function useHarnSession(): SessionStore {
  const store = useContext(HarnSessionContext);
  if (!store) throw new Error("useHarnSession 须在 HarnRuntimeProvider 内使用");
  return store;
}

export function HarnRuntimeProvider({ children }: { children: React.ReactNode }) {
  const [store] = useState(
    () => new SessionStore((message) => harnSocket.send(message)),
  );

  // WS 生命周期与 React effect 解耦（ws.ts 单例 + 自动重连）。
  useEffect(() => {
    const url = HarnSocket.urlFromLocation();
    if (!url) return;
    const unsubscribeMessage = harnSocket.onMessage((message: ServerMsg) => {
      store.dispatch(message);
    });
    // 连接（含重连）成功：整体重建。
    const unsubscribeState = harnSocket.onState((state) => {
      if (state === "open") store.reset();
    });
    harnSocket.connect(url);
    return () => {
      unsubscribeMessage();
      unsubscribeState();
    };
  }, [store]);

  const state = useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getSnapshot,
  );

  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages: state.messages,
    setMessages: (messages) => store.replaceMessages(messages),
    convertMessage: (message) => message,
    isRunning: state.running,
    onNew: async (message) => store.sendUser(extractSendText(message)),
    onCancel: async () => store.requestAbort(),
    onRespondToToolApproval: async (response) => {
      const choice =
        response.approved === true
          ? response.optionId === "allow-always"
            ? "allow_always"
            : "allow_once"
          : "deny";
      store.respondApproval(response.approvalId, choice);
    },
  });

  return (
    <HarnSessionContext.Provider value={store}>
      <AssistantRuntimeProvider runtime={runtime}>{children}</AssistantRuntimeProvider>
    </HarnSessionContext.Provider>
  );
}

/** AppendMessage 可能含多 part/附件；harn v1 只支持纯文本。 */
function extractSendText(message: AppendMessage): string {
  const content = message.content;
  if (typeof content === "string") return content;
  for (const part of content) {
    if (part.type === "text") return part.text;
  }
  throw new Error("暂仅支持文本输入");
}
