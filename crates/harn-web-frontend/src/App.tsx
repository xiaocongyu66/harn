// harn web 应用：HarnRuntimeProvider（WS+store）→ Thread + 弹窗 + 状态栏。

import { HarnThread } from "@/components/harn/ThreadKit";
import { QuestionDialog } from "@/components/harn/QuestionDialog";
import { StatusBar } from "@/components/harn/StatusBar";
import { TokenGate } from "@/components/harn/TokenGate";
import { ToolRegistry } from "@/components/harn/tools";
import { useLocale } from "@/i18n";
import { HarnRuntimeProvider } from "@/runtimes/HarnRuntimeProvider";

export function App() {
  // locale 切换：整个可视树按 key 重挂（store 在 provider 层，不受影响）。
  const locale = useLocale();
  return (
    <HarnRuntimeProvider>
      <AppBody key={locale} />
    </HarnRuntimeProvider>
  );
}

function AppBody() {
  return (
    <>
      <ToolRegistry />
      <div className="mx-auto flex h-screen max-w-3xl flex-col">
        <main className="flex flex-1 flex-col overflow-hidden">
          <TokenGate />
          <HarnThread />
        </main>
        <StatusBar />
      </div>
      <QuestionDialog />
    </>
  );
}
