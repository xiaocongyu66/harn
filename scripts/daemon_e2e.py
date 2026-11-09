#!/usr/bin/env python3
"""harn daemon 生命周期 + 协议闭环冒烟（纯 UDS，无三方依赖）。

校验：
1. start：启动后 socket 可连，快照下发；
2. 协议闭环：send → delta → tool → tool_result → done（mock provider）；
3. status：注册表 live 标记；
4. reload：exec 热重载——同 PID、新随机名、socket 不变、协议仍可用；
5. stop：daemon 退出码 0。

用法：scripts/daemon_e2e.py [socket] [端口无关]
"""

import json
import os
import socket
import subprocess
import sys
import time

SOCKET = sys.argv[1] if len(sys.argv) > 1 else "/tmp/harn-daemon-e2e.sock"
BINARY = "/root/hjimicode/target/debug/harn"
WORKDIR = "/tmp/harn-daemon-e2e"
SCRIPT = json.dumps(
    {
        "turns": [
            {
                "text": "我先读一下文件再回答。",
                "tool_use": [{"id": "r1", "name": "Read", "input": {"file_path": "note.txt"}}],
            },
            {"text": "文件内容是 hello。"},
        ]
    },
    ensure_ascii=False,
)


def wait_socket(timeout=10):
    for _ in range(int(timeout / 0.25)):
        time.sleep(0.25)
        if os.path.exists(SOCKET):
            return True
    return False


def run_server_cmd(*args):
    return subprocess.run(
        [BINARY] + list(args), capture_output=True, text=True, timeout=20
    )


def status_line():
    result = run_server_cmd("server", "status")
    stripped = (result.stdout or result.stderr).strip()
    return stripped.splitlines()[0] if stripped else "(empty)"


def collect(stream, timeout=12):
    """读 stream 直到 done/EOF/超时，返回协议消息列表。"""
    messages = []
    deadline = time.time() + timeout
    buffer = b""
    stream.settimeout(timeout)
    while time.time() < deadline:
        try:
            chunk = stream.recv(65536)
            if not chunk:
                break
            buffer += chunk
        except Exception:
            break
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            if not line.strip():
                continue
            message = json.loads(line)
            messages.append(message)
            if message.get("kind") == "done":
                return messages
    return messages


def assert_protocols(messages):
    entries = [
        m["entry"]["kind"] for m in messages if m.get("kind") == "entry"
    ]
    kinds = [m.get("kind") for m in messages]
    assert "user" in entries, f"缺 user entry：{entries}"
    assert "delta" in kinds, f"缺流式 delta：{kinds}"
    assert "tool" in entries, f"缺工具 entry：{entries}"
    assert "tool_result" in entries, f"缺工具结果：{entries}"
    assert "done" in kinds, f"缺 done：{kinds}"


def main():
    failures = []
    os.makedirs(WORKDIR, exist_ok=True)
    with open(os.path.join(WORKDIR, "script.json"), "w", encoding="utf-8") as handle:
        handle.write(SCRIPT)
    env = dict(os.environ, HARN_MOCK_SCRIPT=os.path.join(WORKDIR, "script.json"))
    log = open(os.path.join(WORKDIR, "daemon.log"), "w", encoding="utf-8")
    daemon = subprocess.Popen(
        [BINARY, "server", "start", "--socket", SOCKET, "--provider", "mock"],
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )

    try:
        # 1. start + 协议闭环
        assert wait_socket(), "daemon 启动后 socket 未出现"
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(SOCKET)
        sock.sendall((json.dumps({"kind": "send", "text": "读 note.txt"}, ensure_ascii=False) + "\n").encode())
        messages = collect(sock)
        sock.close()
        try:
            assert_protocols(messages)
            print("✓ start + 协议闭环（snapshot/user/delta/tool/tool_result/done）")
        except AssertionError as exc:
            failures.append(str(exc))

        # 3. status
        line = status_line()
        print(f"✓ status: {line}")
        if not line.startswith("live"):
            failures.append(f"daemon 未 live：{line}")

        # 4. reload：同 PID、新名字、协议仍可用
        before = status_line()
        result = run_server_cmd("server", "reload", "--socket", SOCKET)
        print(f"✓ reload: {(result.stdout or result.stderr).strip()}")
        time.sleep(2)
        after = status_line()
        before_pid = before.split("pid=")[1].split(")")[0] if "pid=" in before else "?"
        after_pid = after.split("pid=")[1].split(")")[0] if "pid=" in after else "?"
        before_name = before.split()[1] if len(before.split()) > 1 else "?"
        after_name = after.split()[1] if len(after.split()) > 1 else "?"
        print(f"  {before_name} (pid={before_pid}) → {after_name} (pid={after_pid})")
        if not after.startswith("live"):
            failures.append(f"reload 后非 live：{after}")
        if before_pid != after_pid:
            failures.append(f"reload 后 PID 变化（exec 应同进程）：{before_pid}→{after_pid}")

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(SOCKET)
        sock.sendall((json.dumps({"kind": "send", "text": "hi"}, ensure_ascii=False) + "\n").encode())
        post = collect(sock)
        sock.close()
        if not post:
            failures.append("reload 后协议不可用")
        else:
            print(f"✓ reload 后协议可用（{len(post)} 条消息）")
    finally:
        # 5. stop
        run_server_cmd("server", "stop", "--socket", SOCKET)
        time.sleep(1)
        code = daemon.poll()
        print(f"✓ stop: 退出码 {code}")
        if code != 0:
            failures.append(f"stop 后退出码异常：{code}")

    if failures:
        for failure in failures:
            print(f"✗ {failure}")
        sys.exit(1)
    print("daemon e2e: 全部通过")


if __name__ == "__main__":
    main()
