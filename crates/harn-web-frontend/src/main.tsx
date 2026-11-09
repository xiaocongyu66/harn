import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { initTheme } from "@/components/harn/StatusBar";

import "./globals.css";

initTheme();

const root = document.getElementById("root");
if (!root) throw new Error("挂载点 #root 缺失");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
