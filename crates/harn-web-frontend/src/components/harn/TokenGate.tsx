// URL 缺 token 时的引导横幅。

import { t } from "@/i18n";

export function missingToken(): boolean {
  if (typeof location === "undefined") return false;
  return !location.search.includes("token=");
}

export function TokenGate() {
  if (!missingToken()) return null;
  return (
    <div className="bg-destructive/10 text-destructive border-destructive/40 mx-auto mt-4 max-w-2xl rounded-lg border p-3 text-sm">
      {t("token.missing")}{" "}
      <code className="mx-1 rounded bg-muted px-1.5 py-0.5">
        http://127.0.0.1:PORT/?token=…
      </code>
    </div>
  );
}
