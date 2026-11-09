#!/usr/bin/env python3
"""harn --web 协议闭环冒烟（RFC 6455 纯手写客户端，无三方依赖）。

两段校验：
1. 基本闭环：快照 → send → delta → 工具卡 → 工具结果 → done；
2. 权限闭环：默认模式下 mock 脚本含 Write → modal → 回 allow_once → 工具真实执行 → done。

用法：scripts/web_e2e.py [工作目录] [端口]
"""

import base64
import urllib.request
import hashlib
import json
import os
import socket
import struct
import subprocess
import sys
import time

WORKDIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/web-e2e"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 7331

LOOP_SCRIPT = {"turns": [{"text": "我先读一下文件再回答。", "tool_use": [{"id": "r1", "name": "Read", "input": {"file_path": "note.txt"}}]}, {"text": "文件内容是 hello。"}]}
PERM_SCRIPT = {"turns": [{"text": "我写个文件。", "tool_use": [{"id": "w1", "name": "Write", "input": {"file_path": "out.txt", "content": "written\n"}}]}]}


class WsClient:
    def __init__(self, port, token):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=15)
        self.buffer = b""
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            f"GET /ws?token={token} HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        resp = b""
        while b"\r\n\r\n" not in resp:
            resp += self.sock.recv(4096)
        status = resp.split(b"\r\n")[0].decode()
        assert "101" in status, status
        expect = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        assert expect.encode() in resp, "accept-key 不匹配"
        self.buffer = resp[resp.index(b"\r\n\r\n") + 4 :]
        self.sock.settimeout(25)

    def _exact(self, n):
        data, self.buffer = self.buffer[:n], self.buffer[n:]
        while len(data) < n:
            data += self.sock.recv(n - len(data))
        return data

    def read(self):
        header = self._exact(2)
        opcode, masked, length = header[0] & 0x0F, header[1] & 0x80 != 0, header[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", self._exact(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self._exact(8))[0]
        if masked:
            self._exact(4)
        return opcode, self._exact(length) if length else b""

    def send(self, payload):
        data = payload.encode()
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i & 3] for i, b in enumerate(data))
        self.sock.sendall(bytes([0x81, 0x80 | len(data)]) + mask + masked)

    def drain_until(self, predicate, timeout=30):
        deadline = time.time() + timeout
        messages = []
        while time.time() < deadline:
            try:
                opcode, payload = self.read()
            except Exception:
                break
            if opcode == 9:
                self.sock.sendall(bytes([0x8A, 0]))
                continue
            if opcode != 1:
                continue
            message = json.loads(payload)
            messages.append(message)
            if predicate(message):
                return messages
        return messages


def run_server(workdir, script, port, mode=("--permission-mode", "acceptEdits")):
    os.makedirs(workdir, exist_ok=True)
    script_path = os.path.join(workdir, "script.json")
    with open(script_path, "w", encoding="utf-8") as handle:
        json.dump(script, handle, ensure_ascii=False)
    log = open(os.path.join(workdir, "server.log"), "w", encoding="utf-8")
    env = dict(os.environ, HARN_MOCK_SCRIPT=script_path)
    args = ["/root/hjimicode/target/debug/harn", "--web", "--web-port", str(port), "--provider", "mock", *mode]
    process = subprocess.Popen(args, cwd=workdir, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    token = None
    for _ in range(50):
        time.sleep(0.2)
        log.flush()
        content = open(os.path.join(workdir, "server.log"), encoding="utf-8").read()
        if "token=" in content:
            token = content.split("token=")[1].split()[0].strip()
            break
    assert token, "未拿到 token"
    return process, token


def check_loop(client):
    client.send(json.dumps({"kind": "send", "text": "读 note.txt 告诉我内容"}))
    messages = client.drain_until(lambda m: m.get("kind") == "done")
    kinds = [m["kind"] for m in messages]
    entries = [m["entry"]["kind"] for m in messages if m.get("kind") == "entry"]
    assert "user" in entries, f"缺 user entry：{entries}"
    assert "delta" in kinds, f"缺流式 delta：{kinds}"
    assert "tool" in entries, f"缺工具 entry：{entries}"
    assert "tool_result" in entries, f"缺工具结果：{entries}"
    assert "done" in kinds, f"缺 done：{kinds}"


def check_permission(client):
    client.send(json.dumps({"kind": "send", "text": "写 out.txt"}))
    messages = client.drain_until(lambda m: m.get("kind") == "modal")
    modal = next((m for m in messages if m.get("kind") == "modal"), None)
    assert modal, f"未收到权限弹窗：{[m.get('kind') for m in messages]}"
    assert modal["modal"]["kind"] == "permission"
    assert modal["modal"]["tool"] == "Write"
    client.send(json.dumps({"kind": "choose", "reply": {"kind": "permission", "choice": "allow_once"}}))
    messages = client.drain_until(lambda m: m.get("kind") == "done")
    entries = [m["entry"]["kind"] for m in messages if m.get("kind") == "entry"]
    assert "tool_result" in entries, f"审批后未见工具执行：{entries}"


def main():
    failures = []

    process, token = run_server(WORKDIR, LOOP_SCRIPT, PORT)
    try:
        check_loop(WsClient(PORT, token))
        print("✓ 基本闭环：snapshot/delta/tool/tool_result/done")
    except AssertionError as exc:
        failures.append(f"基本闭环：{exc}")
    finally:
        # 静态 dist 检查：首页 200 且含前端挂载点（嵌入 feature 下）。
        with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/", timeout=10) as response:
            html = response.read().decode("utf-8", "replace")
            assert response.status == 200
            # 嵌入构建 → id="root"；非嵌入 → 占位页文案。两种构建都接受。
            assert 'id="root"' in html or '占位页' in html, "首页应为 React 挂载点或占位页"
            print("✓ 静态首页: 200 + 挂载点/占位页")
        process.terminate()

    process, token = run_server(WORKDIR, PERM_SCRIPT, PORT, mode=())
    try:
        check_permission(WsClient(PORT, token))
        print("✓ 权限闭环：modal → allow_once → 真实工具执行")
    except AssertionError as exc:
        failures.append(f"权限闭环：{exc}")
    finally:
        process.terminate()

    if failures:
        for failure in failures:
            print(f"✗ {failure}")
        sys.exit(1)
    print("web e2e: 2/2 passed")


if __name__ == "__main__":
    main()
