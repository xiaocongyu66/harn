//! CLI 集成测试：helper 断言失败即 panic 是预期行为。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

fn workdir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn harn(work: &Path) -> Command {
    let mut command = Command::cargo_bin("harn").expect("binary");
    command
        .current_dir(work)
        .env("HARN_HOME", work.join("home"))
        .env("HARN_PROVIDER", "mock")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .env_remove("HARN_MOCK_SCRIPT")
        .env_remove("HARN_MOCK_SUBAGENT_SCRIPT");
    command
}

fn write_script(work: &Path, name: &str, turns: serde_json::Value) -> PathBuf {
    let path = work.join(name);
    fs::write(
        &path,
        serde_json::to_vec(&json!({ "turns": turns })).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn headless_mock_echo() {
    let work = workdir();
    harn(work.path())
        .args(["-p", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mock: hello"));
}

#[test]
fn writes_file_with_accept_edits() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "writing", "tool_use": [{"name": "Write", "input": {"file_path": "hello.txt", "content": "hello\n"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "write", "--permission-mode", "acceptEdits"])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Write"));
    assert_eq!(
        fs::read_to_string(work.path().join("hello.txt")).unwrap(),
        "hello\n"
    );
}

#[test]
fn plan_mode_blocks_write() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "writing", "tool_use": [{"name": "Write", "input": {"file_path": "blocked.txt", "content": "x"}}]},
            {"text": "kept planning"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "write", "--permission-mode", "plan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("⊘ Write"));
    assert!(!work.path().join("blocked.txt").exists());
}

#[test]
fn plan_approval_then_write() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "proposing", "tool_use": [{"name": "ExitPlanMode", "input": {"plan": "1. write approved.txt\n2. verify"}}]},
            {"text": "executing", "tool_use": [{"name": "Write", "input": {"file_path": "approved.txt", "content": "approved"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "plan", "--permission-mode", "plan", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Plan approved"));
    assert_eq!(
        fs::read_to_string(work.path().join("approved.txt")).unwrap(),
        "approved"
    );
}

#[test]
fn subagent_is_spawned_and_returns_summary() {
    let work = workdir();
    let main = write_script(
        work.path(),
        "main.json",
        json!([
            {"text": "delegating", "tool_use": [{"id": "a1", "name": "Agent", "input": {"description": "explore", "prompt": "find things", "subagent_type": "explore"}}]},
            {"text": "parent done"}
        ]),
    );
    let sub = write_script(
        work.path(),
        "sub.json",
        json!([{ "text": "subagent found 3 files" }]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &main)
        .env("HARN_MOCK_SUBAGENT_SCRIPT", &sub)
        .args(["-p", "delegate", "--permission-mode", "acceptEdits"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[subagent]"))
        .stdout(predicate::str::contains("subagent found 3 files"))
        .stdout(predicate::str::contains("parent done"));
}

#[test]
fn team_runs_parallel_subagents() {
    let work = workdir();
    let main = write_script(
        work.path(),
        "main.json",
        json!([
            {"text": "team", "tool_use": [{"id": "t1", "name": "Team", "input": {"description": "parallel", "tasks": [
                {"name": "one", "prompt": "task one", "subagent_type": "explore"},
                {"name": "two", "prompt": "task two", "subagent_type": "verify"}
            ]}}]},
            {"text": "team done"}
        ]),
    );
    let sub = write_script(
        work.path(),
        "sub.json",
        json!([{ "text": "worker result" }]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &main)
        .env("HARN_MOCK_SUBAGENT_SCRIPT", &sub)
        .args(["-p", "team", "--permission-mode", "acceptEdits"])
        .assert()
        .success()
        .stdout(predicate::str::contains("## one"))
        .stdout(predicate::str::contains("## two"))
        .stdout(predicate::str::contains("team done"));
}

#[test]
fn session_can_be_resumed() {
    let work = workdir();
    let first = harn(work.path())
        .args(["-p", "first message"])
        .output()
        .unwrap();
    assert!(first.status.success(), "first run failed");
    let stderr = String::from_utf8_lossy(&first.stderr);
    let session = stderr
        .split("session=")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("session id")
        .to_string();

    harn(work.path())
        .args(["-p", "second message", "--resume", &session])
        .assert()
        .success()
        .stdout(predicate::str::contains("mock: second message"));

    harn(work.path())
        .args(["sessions", "show", &session])
        .assert()
        .success()
        .stdout(predicate::str::contains("first message"))
        .stdout(predicate::str::contains("second message"));
}

#[test]
fn user_prompt_hook_can_block() {
    let work = workdir();
    let hook = work.path().join("block.sh");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"blocked by hook\"}'\n",
    )
    .unwrap();
    fs::create_dir_all(work.path().join("home")).unwrap();
    fs::write(
        work.path().join("home/settings.json"),
        serde_json::to_vec(&json!({
            "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": format!("bash {}", hook.display())}]}]}
        }))
        .unwrap(),
    )
    .unwrap();
    harn(work.path())
        .args(["-p", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("hook_stopped"))
        .stdout(predicate::str::contains("mock:").not());
}

#[test]
fn pretool_use_hook_can_override_ask() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "touching", "tool_use": [{"name": "Bash", "input": {"command": "touch hook-allowed.txt"}}]},
            {"text": "done"}
        ]),
    );
    let hook = work.path().join("allow.sh");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\"}}'\n",
    )
    .unwrap();
    fs::create_dir_all(work.path().join("home")).unwrap();
    fs::write(
        work.path().join("home/settings.json"),
        serde_json::to_vec(&json!({
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": format!("bash {}", hook.display())}]}]}
        }))
        .unwrap(),
    )
    .unwrap();
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "touch", "--permission-mode", "default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Bash"));
    assert!(work.path().join("hook-allowed.txt").exists());
}

#[test]
fn dangerous_bash_is_denied() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "danger", "tool_use": [{"name": "Bash", "input": {"command": "rm -rf /"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "danger", "--permission-mode", "auto"])
        .assert()
        .success()
        .stdout(predicate::str::contains("⊘ Bash"));
}

#[test]
fn bash_tool_executes() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "run", "tool_use": [{"name": "Bash", "input": {"command": "printf tool-ok"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "run", "--permission-mode", "auto"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tool-ok"));
}

#[test]
fn read_and_edit_tools_work() {
    let work = workdir();
    fs::write(work.path().join("a.txt"), "old value\n").unwrap();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "reading", "tool_use": [{"name": "Read", "input": {"file_path": "a.txt"}}]},
            {"text": "editing", "tool_use": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "old value", "new_string": "new value"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "edit", "--permission-mode", "acceptEdits"])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Read"))
        .stdout(predicate::str::contains("✓ Edit"));
    assert_eq!(
        fs::read_to_string(work.path().join("a.txt")).unwrap(),
        "new value\n"
    );
}

#[test]
fn glob_and_grep_tools_work() {
    let work = workdir();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::write(work.path().join("src/a.rs"), "// TODO: first\n").unwrap();
    fs::write(work.path().join("src/b.txt"), "nothing\n").unwrap();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "glob", "tool_use": [{"name": "Glob", "input": {"pattern": "**/*.rs"}}]},
            {"text": "grep", "tool_use": [{"name": "Grep", "input": {"pattern": "TODO", "path": "src"}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "search"])
        .assert()
        .success()
        .stdout(predicate::str::contains("src/a.rs"))
        .stdout(predicate::str::contains("TODO: first"));
}

#[test]
fn bash_timeout_is_reported() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "sleep", "tool_use": [{"name": "Bash", "input": {"command": "sleep 5", "timeout": 1000}}]},
            {"text": "done"}
        ]),
    );
    harn_i18n::set_locale("en");
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .env("HARN_LOCALE", "en")
        .args(["-p", "timeout", "--permission-mode", "bypassPermissions"])
        .assert()
        .success()
        .stdout(predicate::str::contains(harn_i18n::tr!(
            "tool.bash.timeout",
            timeout_ms = 1000
        )));
}

#[test]
fn bash_large_output_is_truncated_and_saved() {
    let work = workdir();
    let script = write_script(
        work.path(),
        "script.json",
        json!([
            {"text": "big", "tool_use": [{"name": "Bash", "input": {"command": "python3 -c \"print('x'*40000)\""}}]},
            {"text": "done"}
        ]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &script)
        .args(["-p", "big", "--permission-mode", "bypassPermissions"])
        .assert()
        .success();

    let mut found = false;
    for entry in walk(work.path().join("home").as_path()) {
        if entry.extension().and_then(|ext| ext.to_str()) == Some("txt")
            && fs::metadata(&entry)
                .map(|metadata| metadata.len() > 30_000)
                .unwrap_or(false)
        {
            found = true;
        }
    }
    assert!(found, "expected a saved tool-results file");
}

#[test]
fn background_subagents_are_listed() {
    let work = workdir();
    let main = write_script(
        work.path(),
        "main.json",
        json!([
            {"text": "start", "tool_use": [{"id": "b1", "name": "Agent", "input": {"description": "slow explore", "prompt": "take your time", "subagent_type": "explore", "run_in_background": true}}]},
            {"text": "list", "tool_use": [{"id": "b2", "name": "AgentList", "input": {}}]},
            {"text": "done"}
        ]),
    );
    let sub = write_script(
        work.path(),
        "sub.json",
        json!([{"text": "slow result", "delay_ms": 10000}]),
    );
    harn(work.path())
        .env("HARN_MOCK_SCRIPT", &main)
        .env("HARN_MOCK_SUBAGENT_SCRIPT", &sub)
        .args(["-p", "background", "--permission-mode", "acceptEdits"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[STARTED] subagent"))
        .stdout(predicate::str::contains("agent-"))
        .stdout(predicate::str::contains("running"));
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn doctor_and_agents_commands_work() {
    let work = workdir();
    harn(work.path())
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tools:"))
        .stdout(predicate::str::contains("mock"));
    harn(work.path())
        .args(["agents"])
        .assert()
        .success()
        .stdout(predicate::str::contains("explore"));
}
