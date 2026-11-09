#!/usr/bin/env python3
"""PTY 驱动的 TUI 冒烟测试：启动 harn --tui，发送提示，抓取渲染后的屏幕。"""
import fcntl
import os
import pty
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


def screen_text(raw: bytes) -> str:
    text = raw.decode("utf-8", "replace")
    # 去掉 CSI / OSC 序列，只留可见文本
    out, i = [], 0
    while i < len(text):
        if text[i] == "\x1b":
            if i + 1 < len(text) and text[i + 1] == "[":
                j = i + 2
                while j < len(text) and not text[j].isalpha():
                    j += 1
                i = j + 1
                continue
            if i + 1 < len(text) and text[i + 1] == "]":
                j = text.find("\x07", i)
                if j == -1:
                    break
                i = j + 1
                continue
            i += 2
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


def main():
    script_json = sys.argv[1]
    theme_css = sys.argv[2] if len(sys.argv) > 2 else None
    home = sys.argv[3]

    env = dict(os.environ)
    env["HARN_HOME"] = home
    env["HARN_PROVIDER"] = "mock"
    env["HARN_MOCK_SCRIPT"] = script_json
    env["TERM"] = "xterm-256color"
    if theme_css:
        env["HARN_THEME_CSS"] = theme_css

    master, slave = pty.openpty()
    set_winsize(master, 40, 120)
    proc = subprocess.Popen(
        ["./target/debug/harn", "--tui", "--permission-mode", "acceptEdits"],
        stdin=slave, stdout=slave, stderr=slave, env=env, cwd=os.getcwd(),
    )
    os.close(slave)

    chunks = []
    drain(master, 1.0, chunks)
    os.write(master, b"write note.txt\r")
    drain(master, 3.0, chunks)
    os.write(master, b"\x04")  # Ctrl-D 退出
    drain(master, 1.0, chunks)
    proc.wait(timeout=5)

    raw = b"".join(chunks)
    screen = screen_text(raw)
    print(screen)
    print("=== checks ===")
    for label, needle in [
        ("panel title", "harn"),
        ("user echo", "[you]"),
        ("tool line", "[tool]"),
        ("tool result", "[ok]"),
        ("input prompt", "input"),
    ]:
        print(f"{'PASS' if needle in screen else 'FAIL'}  {label}: {needle!r}")
    return 0 if all(
        needle in screen for needle in ["[you]", "[tool]", "[ok]"]
    ) else 1


if __name__ == "__main__":
    sys.exit(main())
