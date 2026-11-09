#!/usr/bin/env python3
"""把本地 git HEAD 通过 GitHub Git Data API 推送（本机 git 通道不可用时用）。

适用环境：api.github.com 可达、github.com git 传输被限速/阻断。
用法：python3 scripts/push_via_api.py <owner/repo> [branch]
"""
import base64, json, subprocess, sys, time

def sh(cmd):
    proc = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"命令失败: {cmd}\n{proc.stderr}")
    return proc.stdout

def gh(method, path, payload=None):
    cmd = ["gh", "api", "--method", method, path, "-H", "Accept: application/vnd.github+json"]
    if payload is not None:
        cmd += ["--input", "-"]
    proc = subprocess.run(
        cmd,
        input=json.dumps(payload) if payload is not None else None,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"{method} {path}: {proc.stderr[:300]}")
    return json.loads(proc.stdout) if proc.stdout.strip() else {}

def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    repo = sys.argv[1]
    branch = sys.argv[2] if len(sys.argv) > 2 else "main"

    # 空仓库先落一个初始文件（Git Data API 拒绝空仓库）
    try:
        gh("GET", f"repos/{repo}/git/refs/heads/{branch}")
    except RuntimeError:
        readme = open("README.md", "rb").read()
        gh("PUT", f"repos/{repo}/contents/README.md", {
            "message": "docs: 初始 README（占位提交）",
            "content": base64.b64encode(readme).decode(),
        })

    entries = {}
    for line in sh("git ls-files -s").strip().splitlines():
        meta, path = line.split("\t", 1)
        mode, sha, _ = meta.split()
        entries[path] = {"mode": mode, "sha": sha}
    for path, entry in entries.items():
        content = sh(f"git cat-file blob {entry['sha']} | base64 -w0")
        try:
            gh("POST", f"repos/{repo}/git/blobs", {"content": content, "encoding": "base64"})
        except RuntimeError:
            pass  # 422 = sha 已存在
        time.sleep(0.05)

    def build_tree(prefix=""):
        files, subs = [], {}
        for path, entry in entries.items():
            if not path.startswith(prefix):
                continue
            rest = path[len(prefix):]
            if "/" in rest:
                head, _ = rest.split("/", 1)
                subs.setdefault(head, prefix + head + "/")
            else:
                files.append((rest, entry))
        out = [
            {
                "path": name,
                "mode": {"100644": "100644", "100755": "100755", "120000": "120000"}.get(entry["mode"], "100644"),
                "type": "blob",
                "sha": entry["sha"],
            }
            for name, entry in files
        ]
        for sub, sub_prefix in subs.items():
            sub_tree = build_tree(sub_prefix)
            if sub_tree:
                created = gh("POST", f"repos/{repo}/git/trees", {"tree": sub_tree})
                out.append({"path": sub, "mode": "040000", "type": "tree", "sha": created["sha"]})
        return out

    tree = gh("POST", f"repos/{repo}/git/trees", {"tree": build_tree()})
    lines = sh("git log -1 --format=%H%n%an%n%ae%n%aI%n%s").split("\n")
    commit = gh("POST", f"repos/{repo}/git/commits", {
        "message": lines[4],
        "tree": tree["sha"],
        "author": {"name": lines[1], "email": lines[2], "date": lines[3]},
    })
    gh("PATCH", f"repos/{repo}/git/refs/heads/{branch}", {"sha": commit["sha"], "force": True})
    print(f"pushed: {commit['sha']} -> {repo}@{branch}")

if __name__ == "__main__":
    main()
