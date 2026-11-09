// 前端 i18n：zh 默认、en 预留；locale 变化即时重渲染。
//
// 与协议的关系：ClientMsg::SetLocale 由前端发出；后端回执 locale 后
// （当前后端不回执）前端自行切换。文案 key 约定见下表。

import { useSyncExternalStore } from "react";

export type Locale = "zh" | "en";

type Messages = Record<string, string>;

const MESSAGES: Record<Locale, Messages> = {
  zh: {
    // composer / thread
    "composer.placeholder": "发送消息…",
    "composer.send": "发送",
    "composer.cancel": "取消发送",
    "composer.stop": "停止生成",
    "composer.resume": "继续生成",
    "composer.voice": "语音输入",
    "composer.voice.stop": "停止听写",
    "thread.voiceConversation": "语音对话",
    "thread.scrollToBottom": "回到底部",
    "thread.copy": "复制",
    "thread.copied": "已复制",
    "thread.regenerate": "重新生成",
    "thread.helpful": "有帮助",
    "thread.notHelpful": "没帮助",
    "thread.more": "更多",
    "thread.edit": "编辑",
    "thread.previous": "上一条",
    "thread.next": "下一条",
    "thread.assistantWorking": "助手工作中",
    // 工具卡
    "tool.running": "工具运行中",
    "tool.waiting": "等待工具",
    "tool.cancelled": "已取消 的工具",
    "tool.failed": "失败 的工具",
    "tool.used": "已使用的工具",
    "tool.queued": "回复已排队，尚未发送",
    "tool.cancelReason": "取消原因：",
    "tool.error": "错误：",
    "tool.approved": "已由用户允许",
    "tool.denied": "已由用户拒绝",
    "tool.allowOnce": "允许一次",
    "tool.allowAlways": "总是允许",
    "tool.deny": "拒绝",
    "tool.denyAlways": "总是拒绝",
    "tool.cancelledBefore": "决定前已取消",
    "tool.expiredBefore": "决定前已过期",
    "tool.answered": "已回答",
    "tool.allowed": "已允许",
    "tool.dismissed": "已忽略",
    "tool.deniedReceipt": "已拒绝",
    "tool.automatic": "（自动）",
    // 审批 / 状态栏
    "approval.allowed": "已允许",
    "approval.refused": "已拒绝",
    "status.connected": "已连接",
    "status.connecting": "连接中…",
    "status.disconnected": "连接已断开，重连中…",
    "status.mode": "模式",
    "mode.default": "默认",
    "mode.acceptEdits": "自动接受编辑",
    "mode.bypassPermissions": "跳过权限",
    "theme.toggle": "切换主题",
    // 弹窗 / 引导
    "question.submit": "提交",
    "question.cancel": "取消",
    "question.multiSelectHint": "可多选",
    "question.defaultHeader": "harn 提问",
    "token.missing": "缺少 token：请从终端打印的 URL 进入（形如",
    // 工具卡骨架
    "tool.emptyOutput": "（空输出）",
  },
  en: {
    "composer.placeholder": "Send a message...",
    "composer.send": "Send",
    "composer.cancel": "Cancel sending",
    "composer.stop": "Stop generating",
    "composer.resume": "Resume generating",
    "composer.voice": "Voice input",
    "composer.voice.stop": "Stop dictation",
    "thread.voiceConversation": "Voice conversation",
    "thread.scrollToBottom": "Scroll to bottom",
    "thread.copy": "Copy",
    "thread.copied": "Copied",
    "thread.regenerate": "Regenerate",
    "thread.helpful": "Helpful",
    "thread.notHelpful": "Not helpful",
    "thread.more": "More",
    "thread.edit": "Edit",
    "thread.previous": "Previous",
    "thread.next": "Next",
    "thread.assistantWorking": "Assistant is working",
    "tool.running": "Running tool",
    "tool.waiting": "Waiting on tool",
    "tool.cancelled": "Cancelled tool",
    "tool.failed": "Failed tool",
    "tool.used": "Used tool",
    "tool.queued": "Reply queued, not sent yet",
    "tool.cancelReason": "Cancelled reason:",
    "tool.error": "Error:",
    "tool.approved": "Approved by user",
    "tool.denied": "User denied tool execution",
    "tool.allowOnce": "Allow",
    "tool.allowAlways": "Always allow",
    "tool.deny": "Deny",
    "tool.denyAlways": "Always deny",
    "tool.cancelledBefore": "Cancelled before a decision",
    "tool.expiredBefore": "Expired before a decision",
    "tool.answered": "Answered",
    "tool.allowed": "Allowed",
    "tool.dismissed": "Dismissed",
    "tool.deniedReceipt": "Denied",
    "tool.automatic": " automatically",
    "approval.allowed": "Allowed",
    "approval.refused": "Denied",
    "status.connected": "Connected",
    "status.connecting": "Connecting…",
    "status.disconnected": "Disconnected, reconnecting…",
    "status.mode": "Mode",
    "mode.default": "Default",
    "mode.acceptEdits": "Accept edits",
    "mode.bypassPermissions": "Bypass permissions",
    "theme.toggle": "Toggle theme",
    "question.submit": "Submit",
    "question.cancel": "Cancel",
    "question.multiSelectHint": "Multiple selection",
    "question.defaultHeader": "harn question",
    "token.missing": "Missing token: open the URL printed in the terminal (like",
    "tool.emptyOutput": "(empty output)",
  },
};

let currentLocale: Locale = "zh";
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getLocale(): Locale {
  return currentLocale;
}

/** 切换 locale（用户操作时同时发 ClientMsg::SetLocale 给后端）。 */
export function setLocale(locale: Locale) {
  if (locale === currentLocale) return;
  currentLocale = locale;
  for (const listener of listeners) listener();
}

export function t(key: string): string {
  return MESSAGES[currentLocale][key] ?? MESSAGES.zh[key] ?? key;
}

/** 组件内用：locale 变化时重渲染。 */
export function useT(): (key: string) => string {
  useSyncExternalStore(subscribe, getLocale, getLocale);
  return t;
}

/** React 组件外可用的组件辅助：绑定 t 到当前 locale。 */
export function useLocale(): Locale {
  return useSyncExternalStore(subscribe, getLocale, getLocale);
}
