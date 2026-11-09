// 状态栏：连接状态灯 + 权限模式切换 + 深浅主题切换。

import { MoonIcon, SunIcon } from "lucide-react";
import { useSyncExternalStore } from "react";

import { setLocale, t, type Locale } from "@/i18n";

import { Button } from "@/components/ui/button";
import { harnSocket } from "@/lib/ws";
import { useHarnSession } from "@/runtimes/HarnRuntimeProvider";

type Theme = "light" | "dark";

const MODES: ReadonlyArray<{ value: string; key: string }> = [
  { value: "default", key: "mode.default" },
  { value: "acceptEdits", key: "mode.acceptEdits" },
  { value: "bypassPermissions", key: "mode.bypassPermissions" },
];

export function StatusBar() {
  const store = useHarnSession();
  const state = useSyncExternalStore(
    harnSocket.onState.bind(harnSocket),
    () => harnSocket.state,
    () => harnSocket.state,
  );
  const theme = useTheme();

  const light = {
    color: state === "open" ? "#22c55e" : state === "connecting" ? "#eab308" : "#ef4444",
  };

  return (
    <footer className="border-t flex items-center gap-3 px-4 py-2 text-xs">
      <span className="flex items-center gap-1.5">
        <span
          style={light}
          className="inline-block size-2 rounded-full"
          aria-label={state}
        />
        <span className="text-muted-foreground">
          {state === "open"
            ? t("status.connected")
            : state === "connecting"
              ? t("status.connecting")
              : t("status.disconnected")}
        </span>
      </span>

      <label className="text-muted-foreground flex items-center gap-1">
        {t("status.mode")}
        <select
          className="bg-muted rounded border px-1.5 py-0.5"
          defaultValue="acceptEdits"
          onChange={(event) => store.setMode(event.target.value)}
        >
          {MODES.map((mode) => (
            <option key={mode.value} value={mode.value}>
              {t(mode.key)}
            </option>
          ))}
        </select>
      </label>

      <select
        className="bg-muted rounded border px-1.5 py-0.5"
        defaultValue="zh"
        onChange={(event) => {
          const locale = event.target.value as Locale;
          setLocale(locale);
          store.setLocale(locale);
        }}
        aria-label="locale"
      >
        <option value="zh">中文</option>
        <option value="en">English</option>
      </select>

      <Button
        variant="ghost"
        size="sm"
        className="ml-auto"
        onClick={() => applyTheme(theme === "dark" ? "light" : "dark")}
        aria-label={t("theme.toggle")}
      >
        {theme === "dark" ? <SunIcon className="size-4" /> : <MoonIcon className="size-4" />}
      </Button>
    </footer>
  );
}

function useTheme(): Theme {
  // 简化订阅：切换主题时由 applyTheme 通知监听者。
  return useSyncExternalStore(subscribeTheme, readTheme, readTheme);
}

const themeListeners = new Set<() => void>();

function subscribeTheme(listener: () => void): () => void {
  themeListeners.add(listener);
  return () => themeListeners.delete(listener);
}

function readTheme(): Theme {
  if (typeof document === "undefined") return "dark";
  return document.documentElement.classList.contains("dark") ? "dark" : "light";
}

export function applyTheme(theme: Theme) {
  const root = document.documentElement;
  root.classList.toggle("dark", theme === "dark");
  localStorage.setItem("harn-web-theme", theme);
  for (const listener of themeListeners) listener();
}

/** 启动主题：localStorage > prefers-color-scheme（默认深色）。 */
export function initTheme() {
  if (typeof document === "undefined") return;
  const saved = localStorage.getItem("harn-web-theme");
  if (saved === "light" || saved === "dark") {
    applyTheme(saved);
    return;
  }
  const preferDark = window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? true;
  applyTheme(preferDark ? "dark" : "light");
}
