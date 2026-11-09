// Modal{Question} 的独立弹窗（harn 的提问/确认不保证挂在 tool part 上）。

import { useEffect, useState, useSyncExternalStore } from "react";
import { t } from "@/i18n";

import { Button } from "@/components/ui/button";
import type { SessionState } from "@/lib/session";
import { useHarnSession } from "@/runtimes/HarnRuntimeProvider";
import { cn } from "@/lib/utils";

export function QuestionDialog() {
  const store = useHarnSession();
  const question = useSessionSelector(store, (state) => state.question);

  const [selected, setSelected] = useState<Set<string>>(new Set());

  // 每次新提问重置选区。
  useEffect(() => {
    setSelected(new Set());
  }, [question?.question]);

  if (!question) return null;

  const toggle = (label: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(label)) next.delete(label);
      else next.add(label);
      return next;
    });
  };

  const submit = () => {
    if (selected.size === 0) {
      store.respondQuestion(null);
      return;
    }
    // harn 协议：answers 为 Vec<String>（单选也包一层）。
    store.respondQuestion([...selected]);
  };
  const dismiss = () => store.respondQuestion(null);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4">
      <div className="bg-popover text-popover-foreground w-full max-w-lg rounded-xl border p-6 shadow-lg">
        <h2 className="text-lg font-semibold">{question.header || t("question.defaultHeader")}</h2>
        <p className="text-muted-foreground mt-1 text-sm">{question.question}</p>

        <div className="mt-4 flex flex-col gap-2">
          {question.options.map((option) => {
            const active = selected.has(option.label);
            return (
              <button
                key={option.label}
                type="button"
                onClick={() =>
                  question.multiSelect
                    ? toggle(option.label)
                    : setSelected(new Set([option.label]))
                }
                className={cn(
                  "flex flex-col rounded-lg border px-4 py-3 text-start transition-colors",
                  active ? "border-primary bg-primary/10" : "hover:bg-accent border-border",
                )}
              >
                <span className="font-medium">{option.label}</span>
                {option.description && (
                  <span className="text-muted-foreground text-sm">{option.description}</span>
                )}
              </button>
            );
          })}
        </div>

        {question.multiSelect && (
          <p className="text-muted-foreground mt-2 text-xs">{t("question.multiSelectHint")}</p>
        )}

        <div className="mt-6 flex justify-end gap-2">
          <Button variant="ghost" onClick={dismiss}>
            {t("question.cancel")}
          </Button>
          <Button onClick={submit} disabled={selected.size === 0}>
            {t("question.submit")}
          </Button>
        </div>
      </div>
    </div>
  );
}

/** 按 selector 订阅 store（弹窗等小组件避免整 state 变化引起的重渲染）。 */
function useSessionSelector<T>(
  store: ReturnType<typeof useHarnSession>,
  selector: (state: SessionState) => T,
): T {
  return useSyncExternalStore(
    store.subscribe,
    () => selector(store.getSnapshot()),
    () => selector(store.getSnapshot()),
  );
}
