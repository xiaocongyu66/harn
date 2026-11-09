#!/usr/bin/env python3
"""PTY 驱动的完整 TUI 冒烟测试：输入/历史/补全/帮助/侧栏/弹层/工具生命周期。

用法: tui_full_smoke.py <script.json> <home>

断言方式：内置一个最小终端屏幕模拟器（CSI 光标移动/清屏/EL + 宽字符 +
回车换行），把 PTY 输出喂进去得到**真实最终屏幕**再比对。ratatui 是 diff
渲染——相同前缀的单元格跨帧不重印，朴素拼接字节流会把 "/compact" 断成
"/c"+"ompact" 之类的碎片（曾稳定误报），屏幕状态才是可信来源。
"""
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time


def set_winsize(fd, rows, cols):
    """PTY 默认 0x0，ratatui 会拿到空画布；必须显式设置窗口大小。"""
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def drain(fd, seconds, out):
    end = time.time() + seconds
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            try:
                out.append(os.read(fd, 65536))
            except OSError:
                return


CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?")


def char_width(ch):
    """全角计 2，其余计 1（与 ratatui 的宽字符处理一致）。"""
    return 2 if ord(ch) >= 0x2E80 else 1


class Screen:
    """最小终端屏幕：够解析 ratatui/crossterm 的输出（CUP/CU*/ED/EL/SGR）。

    宽字符占两格：第二格标记为 continuation，text() 不输出；否则 diff 渲染
    下被跳过的续格会留着上一帧的残字（真实终端不会，模拟器必须处理）。
    """

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.wide = [[False] * cols for _ in range(rows)]
        self.r = self.c = 0

    def _erase_line_from_cursor(self):
        for c in range(self.c, self.cols):
            self.grid[self.r][c] = " "
            self.wide[self.r][c] = False

    def _erase_line_all(self):
        self.grid[self.r] = [" "] * self.cols
        self.wide[self.r] = [False] * self.cols

    def _erase_display_from_cursor(self):
        self._erase_line_from_cursor()
        for r in range(self.r + 1, self.rows):
            self.grid[r] = [" "] * self.cols
            self.wide[r] = [False] * self.cols

    def _erase_display_all(self):
        for r in range(self.rows):
            self.grid[r] = [" "] * self.cols
            self.wide[r] = [False] * self.cols

    def _put(self, ch):
        if self.r >= self.rows or self.c >= self.cols:
            return
        step = char_width(ch)
        self.grid[self.r][self.c] = ch
        self.wide[self.r][self.c] = False
        if step == 2 and self.c + 1 < self.cols:
            self.grid[self.r][self.c + 1] = " "
            self.wide[self.r][self.c + 1] = True
        self.c = min(self.c + step, self.cols - 1)

    def _csi(self, params, final):
        nums = [int(p) for p in params.replace("?", "").split(";") if p.isdigit()]
        if final in ("H", "f"):  # CUP：1-based
            r = (nums[0] - 1) if len(nums) >= 1 else 0
            c = (nums[1] - 1) if len(nums) >= 2 else 0
            self.r = min(max(r, 0), self.rows - 1)
            self.c = min(max(c, 0), self.cols - 1)
        elif final == "J":  # ED
            mode = nums[0] if nums else 0
            if mode == 2:
                self._erase_display_all()
            else:
                self._erase_display_from_cursor()
        elif final == "K":  # EL
            mode = nums[0] if nums else 0
            if mode == 2:
                self._erase_line_all()
            else:
                self._erase_line_from_cursor()
        elif final == "A":
            self.r = max(self.r - (nums[0] if nums else 1), 0)
        elif final == "B":
            self.r = min(self.r + (nums[0] if nums else 1), self.rows - 1)
        elif final == "C":
            self.c = min(self.c + (nums[0] if nums else 1), self.cols - 1)
        elif final == "D":
            self.c = max(self.c - (nums[0] if nums else 1), 0)
        # 其余（SGR 等）不影响屏幕内容

    def feed(self, data: bytes):
        text = data.decode("utf-8", "replace")
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                m = CSI.match(text, i)
                if m:
                    self._csi(m.group(1), m.group(2))
                    i = m.end()
                    continue
                m = OSC.match(text, i)
                if m:
                    i = m.end()
                    continue
                i += 2  # 其他两字节转义
                continue
            if ch == "\r":
                self.c = 0
                i += 1
                continue
            if ch == "\n":
                self.r = min(self.r + 1, self.rows - 1)
                i += 1
                continue
            if ch == "\x08":
                self.c = max(self.c - 1, 0)
                i += 1
                continue
            self._put(ch)
            i += 1

    def text(self):
        return "\n".join(
            "".join(ch for ch, is_wide in zip(row, flags) if not is_wide).rstrip()
            for row, flags in zip(self.grid, self.wide)
        )


def main():
    script, home = sys.argv[1], sys.argv[2]
    work = os.path.dirname(os.path.abspath(script))
    env = dict(os.environ)
    env.update(HARN_HOME=home, HARN_PROVIDER="mock", HARN_MOCK_SCRIPT=script, TERM="xterm-256color")

    question_script = os.path.join(work, "question.json")
    with open(question_script, "w") as handle:
        json.dump({"turns": [
            {"text": "asking", "tool_use": [{"name": "AskUserQuestion", "input": {"questions": [{
                "question": "Which deploy target?", "header": "Deploy",
                "options": [{"label": "staging", "description": "safe"}, {"label": "prod"}]
            }]}}]},
            {"text": "answered-and-ran"}
        ]}, handle)

    master, slave = pty.openpty()
    set_winsize(master, 45, 130)
    proc = subprocess.Popen(
        [os.path.abspath("./target/debug/harn"), "--tui", "--permission-mode", "acceptEdits"],
        stdin=slave, stdout=slave, stderr=slave, env=env, cwd=work, close_fds=True,
    )
    os.close(slave)

    chunks = []
    checks = []
    screen = Screen(45, 130)

    drain(master, 3.0, chunks)  # 等 debug 二进制完成启动再发按键
    screen.feed(b"".join(chunks))

    def press(key, label, needle, wait=1.6, absent=False, accumulate=True, settle=0.0):
        """发送一个按键并对**屏幕当前状态**断言。

        accumulate 参数保留以兼容旧调用点；屏幕模拟器天然携带历史状态，
        不再需要“只看本次重绘窗口”的区分。
        """
        if settle:
            drain(master, settle, chunks)
            screen.feed(b"".join(chunks[-1:]) if chunks else b"")
        ret = os.write(master, key)
        window = []
        drain(master, wait, window)
        chunks.extend(window)
        screen.feed(b"".join(window))
        if os.environ.get("SMOKE_DEBUG"):
            print(f"press {label!r}: wrote {ret}, window {len(b''.join(window))} bytes")
        current = screen.text()
        hit = needle in current
        checks.append((label, (not hit) if absent else hit))

    # 1) 提示词 + 工具循环（含工具生命周期与并发前的单调用）
    press(b"write note.txt\r", "用户回显", "❯ write note.txt", 2.2)
    if os.environ.get("SMOKE_DEBUG"):
        print("startup bytes:", len(b"".join(chunks)))
    screen_now = screen.text()
    checks.append(("工具开始标记", "Write" in screen_now))
    checks.append(("工具结果为 ok", "Wrote" in screen_now))

    # 2) 斜杠补全：/co → 候选 → 回车补全（前缀匹配短名优先 → 首项 /cost）
    press(b"/co", "斜杠补全弹出", "/compact", 1.2)
    press(b"\r", "补全被接受", "/cost", 1.2)

    # 3) 历史：↑ 召回上一条
    press(b"\x1b[A", "历史召回", "❯ write note.txt", 1.2)

    # 4) 帮助浮层 + 翻页 + 关闭
    press(b"\x1bOP", "帮助浮层", "help (Esc", 1.2)
    press(b"\x1b[6~", "帮助可翻页", "Esc: close", 1.0)
    press(b"\x1b", "帮助关闭", "keybinding", 1.0, absent=True)

    # 5) 侧栏（Tab 切换；出现/消失用窗口判定）
    press(b"\t", "侧栏显示", "sidebar", 1.4)
    press(b"\t", "侧栏隐藏", "sidebar", 1.4, absent=True)

    # 6) 权限弹层（default 模式下再次触发）
    os.write(master, b"\x15")  # Ctrl-U 清空输入，保证 Ctrl-D 是退出而非删字符
    drain(master, 0.5, chunks)
    os.write(master, b"\x04")  # Ctrl-D 退出
    drain(master, 1.0, chunks)
    proc.wait(timeout=5)

    screen_now = screen.text()
    for label, needle in [
        ("输入提示符", "❯"),
        ("状态栏模型", "mock-model"),
        ("流式文本", "Wrote"),
        ("运行/空闲态", "idle"),
        ("模式徽标", "⏵⏵"),
        ("用户行色块命中", "❯ write note.txt"),
    ]:
        checks.append((label, needle in screen_now))

    # 7) default 模式弹层验证（单独跑一次）
    env2 = dict(env)
    env2["HARN_HOME"] = home + "-2"
    master2, slave2 = pty.openpty()
    set_winsize(master2, 40, 120)
    proc2 = subprocess.Popen(
        [os.path.abspath("./target/debug/harn"), "--tui", "--permission-mode", "default"],
        stdin=slave2, stdout=slave2, stderr=slave2, env=env2, cwd=work, close_fds=True,
    )
    os.close(slave2)
    out2 = []
    screen2_model = Screen(40, 120)
    drain(master2, 1.0, out2)
    screen2_model.feed(b"".join(out2))
    os.write(master2, b"write note.txt\r")
    drain(master2, 1.5, out2)
    screen2_model.feed(b"".join(out2))
    screen2 = screen2_model.text()
    checks.append(("权限弹层标题", "permission required" in screen2))
    checks.append(("弹层含允许一次", "Allow once" in screen2))
    os.write(master2, b"\r")  # 允许一次
    drain(master2, 2.0, out2)
    screen2_model.feed(b"".join(out2))
    screen2 = screen2_model.text()
    checks.append(("批准后执行", "Wrote" in screen2))
    os.write(master2, b"\x04")
    drain(master2, 1.0, out2)
    proc2.wait(timeout=5)

    # 8) 提问弹层：AskUserQuestion → 选择 → 模型继续
    env3 = dict(env)
    env3["HARN_HOME"] = home + "-3"
    env3["HARN_MOCK_SCRIPT"] = question_script
    master3, slave3 = pty.openpty()
    set_winsize(master3, 40, 120)
    proc3 = subprocess.Popen(
        [os.path.abspath("./target/debug/harn"), "--tui", "--permission-mode", "acceptEdits"],
        stdin=slave3, stdout=slave3, stderr=slave3, env=env3, cwd=work, close_fds=True,
    )
    os.close(slave3)
    out3 = []
    screen3_model = Screen(40, 120)
    drain(master3, 1.0, out3)
    screen3_model.feed(b"".join(out3))
    os.write(master3, b"ask\r")
    drain(master3, 2.0, out3)
    screen3_model.feed(b"".join(out3))
    screen3 = screen3_model.text()
    checks.append(("提问弹层标题", "ask user" in screen3))
    checks.append(("提问内容", "Which deploy target?" in screen3))
    checks.append(("提问选项", "staging" in screen3))
    checks.append(("取消项", "Dismiss" in screen3))
    os.write(master3, b"\r")  # 选择当前项（staging）
    drain(master3, 2.0, out3)
    screen3_model.feed(b"".join(out3))
    screen3 = screen3_model.text()
    checks.append(("选择后继续", "answered-and-ran" in screen3))
    os.write(master3, b"\x04")
    drain(master3, 1.0, out3)
    proc3.wait(timeout=5)

    failed = [label for label, ok in checks if not ok]
    for label, ok in checks:
        print(f"{'PASS' if ok else 'FAIL'}  {label}")
    print(f"\n{len(checks) - len(failed)}/{len(checks)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
