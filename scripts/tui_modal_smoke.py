#!/usr/bin/env python3
"""PTY 驱动的权限弹层冒烟测试：default 模式下 Write 触发询问弹层，回车选择「允许一次」。"""
import fcntl
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


def strip(raw: bytes) -> str:
    text = raw.decode("utf-8", "replace")
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
                i = (j + 1) if j != -1 else len(text)
                continue
            i += 2
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


def main():
    home, script = sys.argv[1], sys.argv[2]
    target = sys.argv[3]
    env = dict(os.environ)
    env.update(
        HARN_HOME=home,
        HARN_PROVIDER="mock",
        HARN_MOCK_SCRIPT=script,
        TERM="xterm-256color",
    )
    master, slave = pty.openpty()
    set_winsize(master, 40, 120)
    work = os.path.dirname(script)
    proc = subprocess.Popen(
        [os.path.abspath("./target/debug/harn"), "--tui", "--permission-mode", "default"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        cwd=work,
        close_fds=True,
    )
    os.close(slave)

    chunks = []
    drain(master, 1.0, chunks)
    os.write(master, b"write it\r")
    drain(master, 1.5, chunks)
    modal = b"".join(chunks)
    os.write(master, b"\r")  # 「允许一次」
    drain(master, 2.5, chunks)
    os.write(master, b"\x04")
    drain(master, 1.0, chunks)
    proc.wait(timeout=5)

    screen = strip(b"".join(chunks))
    print(screen[-1600:])
    print("=== checks ===")
    for label, needle in [
        ("modal title", "permission required"),
        ("tool name", "Write"),
        ("option allow once", "Allow once"),
        ("option deny", "Deny"),
        ("tool executed afterwards", "[ok] Write"),
    ]:
        print(f"{'PASS' if needle in screen else 'FAIL'}  {label}: {needle!r}")
    ok = os.path.exists(target)
    print(f"{'PASS' if ok else 'FAIL'}  file written: {target}")
    return 0 if ("Allow once" in screen and "[ok] Write" in screen and ok) else 1


if __name__ == "__main__":
    sys.exit(main())
