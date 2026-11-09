"""内嵌的最小 LSP 服务器（harn-tools 集成测试专用）。

不是玩具级别的假响应：它实现了 LSP 3.17 的核心交互闭环——
Content-Length 帧、initialize 握手、didOpen 触发 publishDiagnostics、
definition/references/hover/documentSymbol/workspaceSymbol/
callHierarchy 三连、shutdown/exit 退出。返回固定数据，但真实走 stdio。
"""
import json
import sys


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("ascii", "replace").strip()
        if not line:
            break
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8"))


def send(payload):
    data = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n")
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()


def handle(message):
    method = message.get("method")
    msg_id = message.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "capabilities": {
                    "positionEncoding": "utf-8",
                    "textDocumentSync": 1,
                    "definitionProvider": True,
                    "referencesProvider": True,
                    "hoverProvider": True,
                    "documentSymbolProvider": True,
                    "workspaceSymbolProvider": True,
                    "callHierarchyProvider": True,
                },
                "serverInfo": {"name": "harn-mock-lsp", "version": "0.1.0"},
            },
        })
        return
    if method == "initialized":
        return
    if method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg_id, "result": None})
        return
    if method == "exit":
        sys.exit(0)
    if method == "textDocument/didOpen" or method == "textDocument/didChange":
        # 文档变更后立刻推送一条固定诊断（联调 publishDiagnostics 通道）
        uri = message["params"]["textDocument"]["uri"]
        send({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3},
                    },
                    "severity": 1,
                    "source": "mock-lsp",
                    "message": "mock: symbol not found",
                }],
            },
        })
        return
    if method == "textDocument/definition":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {"uri": "file:///mock/target.rs", "range": {
                "start": {"line": 9, "character": 0},
                "end": {"line": 9, "character": 4},
            }},
        })
        return
    if method == "textDocument/references":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [
                {"uri": "file:///mock/a.rs", "range": {
                    "start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}},
                {"uri": "file:///mock/b.rs", "range": {
                    "start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 4}}},
            ],
        })
        return
    if method == "textDocument/hover":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "contents": {"kind": "markdown", "value": "fn mock(a: u32) -> u32\n\nThe mock symbol."},
            },
        })
        return
    if method == "textDocument/documentSymbol":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "mock_symbol",
                "kind": 12,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 8}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 8}},
                "children": [{
                    "name": "inner",
                    "kind": 13,
                    "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                    "selectionRange": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}},
                }],
            }],
        })
        return
    if method == "workspace/symbol":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "MockThing",
                "kind": 5,
                "location": {"uri": "file:///mock/ws.rs", "range": {
                    "start": {"line": 7, "character": 0}, "end": {"line": 7, "character": 9}}},
                "containerName": "mock crate",
            }],
        })
        return
    if method == "textDocument/prepareCallHierarchy":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "caller",
                "kind": 12,
                "uri": "file:///mock/call.rs",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 6}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 6}},
            }],
        })
        return
    if method == "callHierarchy/incomingCalls" or method == "callHierarchy/outgoingCalls":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "to": {
                    "name": "callee",
                    "kind": 12,
                    "uri": "file:///mock/call2.rs",
                    "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 6}},
                    "selectionRange": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 6}},
                },
                "fromRanges": [{"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 12}}],
            }],
        })
        return
    if method == "shutdown/unknown":
        return
    # server→client 请求示例：回 method not found
    send({"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": "mock: unsupported"}})


def main():
    while True:
        message = read_message()
        if message is None:
            return
        try:
            handle(message)
        except SystemExit:
            raise
        except Exception as error:  # noqa: BLE001
            if message.get("id") is not None:
                send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": str(error)}})


if __name__ == "__main__":
    main()
