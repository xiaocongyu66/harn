//! EnterWorktree / ExitWorktree：会话级工作区切换（对齐 CC 的两个工具）。
//!
//! 与 `Agent(isolation:"worktree")` 的分工：后者在**派发时**为子代理建工作区
//! （父不动）；本对工具让**主代理自己**切进/退出工作区，会话内后续所有工具调用
//! 以 worktree 为根。合并只给 git 命令建议，不自动执行。

use async_trait::async_trait;
use harn_core::error::{HarnError, Result};
use serde_json::{Value, json};

use super::MergeMode;
use crate::{Tool, ToolContext, ToolOutput, object_schema};

pub struct EnterWorktreeTool;

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn name(&self) -> &'static str {
        "EnterWorktree"
    }

    fn description(&self) -> &'static str {
        "Create (or rejoin) an isolated git worktree at .harn/worktrees/<name> inside the \
         project and switch this session into it: all later file operations run in that \
         working copy, the main checkout stays untouched. Give a short slug ([a-z0-9._-], \
         no ..), or omit it for a random one. Requires the project to be a git repository."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "name": {
                    "type": "string",
                    "description": "Worktree slug; segments may contain letters, digits, dots, \
                     underscores, dashes. Omit for a random name. Max 64 chars."
                }
            }),
            &[],
        )
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "子代理不能自行切换工作区；隔离的子代理工作区由父级 Agent 调用的 \
                 isolation 参数控制。把需要隔离的部分交回主代理。",
            ));
        }
        let Some(manager) = &ctx.worktrees else {
            return Ok(ToolOutput::error(
                "未初始化工作区管理器（本项目不是 git 仓库）。",
            ));
        };
        if let Some(current) = ctx
            .session_worktree
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            return Ok(ToolOutput::error(format!(
                "本会话已在工作区 `{}`（{}）。先 ExitWorktree 再进入新的工作区。",
                current.slug,
                current.path.display()
            )));
        }
        let slug = match input.get("name").and_then(Value::as_str).map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => super::random_slug(),
        };
        let worktree = manager.create(&slug).await?;
        let created_line = if worktree.created {
            "已创建新工作区".to_string()
        } else {
            "复用了已存在的工作区".to_string()
        };
        if let Ok(mut slot) = ctx.session_worktree.lock() {
            *slot = Some(worktree.clone());
        }
        Ok(ToolOutput::ok(format!(
            "{created_line}：{path}\n分支：{branch}（基线 {base}）。\n\
             本会话已切换到该工作区：后续文件操作默认以它为根，主工作树不受影响。\n\
             完成后调用 ExitWorktree：action=keep 保留并给出合并命令，\
             action=remove 删除（有改动且未 discardChanges 时会被拒绝）。",
            path = worktree.path.display(),
            branch = worktree.branch,
            base = worktree.base,
        )))
    }
}

pub struct ExitWorktreeTool;

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &'static str {
        "ExitWorktree"
    }

    fn description(&self) -> &'static str {
        "Leave the current worktree and switch back to the main checkout. \
         action=keep: keep the worktree and return change counts plus git merge commands; \
         action=remove: delete the worktree and its branch — refused when the worktree has \
         uncommitted files or unmerged commits unless discardChanges=true (fail-closed)."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "action": {
                    "type": "string",
                    "enum": ["keep", "remove"],
                    "description": "keep = 保留工作区并给出合并命令；remove = 删除工作区与分支。"
                },
                "discardChanges": {
                    "type": "boolean",
                    "description": "remove 时显式为 true 才允许销毁有未提交/未合并改动的工作区。"
                }
            }),
            &["action"],
        )
    }

    fn is_read_only(&self, input: &Value) -> bool {
        // keep 不销毁任何东西；remove 是毁灭性操作。
        !matches!(input.get("action").and_then(Value::as_str), Some("remove"))
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "子代理不管理工作区生命周期；把结果返回给主代理，由主代理 ExitWorktree。",
            ));
        }
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| HarnError::Tool("ExitWorktree 需要 `action`（keep|remove）".into()))?;
        let discard = input
            .get("discardChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(manager) = &ctx.worktrees else {
            return Ok(ToolOutput::error("未初始化工作区管理器。"));
        };
        let current = ctx
            .session_worktree
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(worktree) = current else {
            return Ok(ToolOutput::error(
                "本会话不在工作区中（没有可退出的 worktree）。",
            ));
        };

        let summary = match action {
            "keep" => manager.keep(&worktree).await?,
            "remove" => {
                manager.remove(&worktree, discard).await?;
                // remove 成功：清槽位，回报变更消失
                if let Ok(mut slot) = ctx.session_worktree.lock() {
                    slot.take();
                }
                return Ok(ToolOutput::ok(format!(
                    "已删除工作区 {path}（分支 {branch}），会话回到主工作树 {cwd}。",
                    path = worktree.path.display(),
                    branch = worktree.branch,
                    cwd = ctx.cwd.display(),
                )));
            }
            other => {
                return Err(HarnError::Tool(format!(
                    "未知 action：{other:?}（可用 keep / remove）"
                )));
            }
        };

        if let Ok(mut slot) = ctx.session_worktree.lock() {
            slot.take();
        }
        let mut out = format!(
            "已保留工作区 {path}（分支 {branch}），会话回到主工作树 {cwd}。",
            path = worktree.path.display(),
            branch = worktree.branch,
            cwd = ctx.cwd.display(),
        );
        out.push_str(&format!(
            "\n改动统计：{} 个未提交文件，{} 个未合并提交。",
            summary.changes.files, summary.changes.commits
        ));
        if let Some(hint) = &summary.merge_hint {
            out.push_str(&format!(
                "\n合并回主项目（自行选择其一执行）：\n  {}\n  {}",
                hint.cherry_pick, hint.merge
            ));
        } else {
            out.push_str("\n工作区无改动，可直接丢弃。");
        }
        Ok(ToolOutput::ok(out))
    }
}

pub struct MergeWorktreeTool;

#[async_trait]
impl Tool for MergeWorktreeTool {
    fn name(&self) -> &'static str {
        "MergeWorktree"
    }

    fn description(&self) -> &'static str {
        "Merge an isolated worktree back into the main checkout's current branch. \
         mode=cherry-pick (default) replays the worktree's commits linearly; \
         mode=merge creates one --no-ff merge commit. Conflicts abort automatically \
         and roll the main checkout back — a conflicted merge never leaves a \
         half-merged tree behind. Set dryRun=true first to list the commits and \
         preconditions without touching anything. The main checkout must be clean; \
         uncommitted worktree changes are not carried over (merge commits only)."
    }

    fn input_schema(&self) -> Value {
        object_schema(
            json!({
                "name": {
                    "type": "string",
                    "description": "Worktree slug. Omit to merge the worktree this session is in."
                },
                "mode": {
                    "type": "string",
                    "enum": ["cherry-pick", "merge"],
                    "description": "cherry-pick = 线性重放提交（默认）；merge = --no-ff 合并提交。"
                },
                "dryRun": {
                    "type": "boolean",
                    "description": "true = 只列出将进入的提交与前置检查，不执行合并。"
                }
            }),
            &[],
        )
    }

    fn is_read_only(&self, input: &Value) -> bool {
        input
            .get("dryRun")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.depth > 0 {
            return Ok(ToolOutput::error(
                "子代理不执行合并（合并必须发生在主工作树）；把结果返回给主代理，由其 MergeWorktree。",
            ));
        }
        let Some(manager) = &ctx.worktrees else {
            return Ok(ToolOutput::error(
                "未初始化工作区管理器（本项目不是 git 仓库）。",
            ));
        };
        let mode = match input.get("mode").and_then(Value::as_str) {
            None | Some("") | Some("cherry-pick") => MergeMode::CherryPick,
            Some("merge") => MergeMode::Merge,
            Some(other) => {
                return Ok(ToolOutput::error(format!(
                    "未知 mode：{other:?}（可用 cherry-pick / merge）"
                )));
            }
        };
        let dry_run = input
            .get("dryRun")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // 目标工作区：入参 name > 会话当前工作区；都没有则报错（不让模型猜）。
        let worktree = match input.get("name").and_then(Value::as_str).map(str::trim) {
            Some(name) if !name.is_empty() => {
                let worktrees = manager.list().await?;
                worktrees
                    .into_iter()
                    .find(|entry| entry.slug == name)
                    .ok_or_else(|| {
                        HarnError::Tool(format!(
                            "找不到工作区 `{name}`（用 EnterWorktree 建的才会出现在列表里）"
                        ))
                    })?
            }
            _ => ctx
                .session_worktree
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .ok_or_else(|| {
                    HarnError::Tool(
                        "未指定 name 且本会话不在工作区：给出要合并的工作区名，或先 EnterWorktree"
                            .into(),
                    )
                })?,
        };

        if dry_run {
            let plan = manager.merge_plan(&worktree, mode).await?;
            return Ok(ToolOutput::ok(format!(
                "[DRY-RUN] {}（分支 {}）→ 主工作树分支 {}，mode={}\n将要进入的提交（{} 个）：\n{}\n主工作树未提交文件：{}；worktree 未提交文件：{}（未提交内容不带入合并）。\n确认无误后去掉 dryRun 再执行。",
                plan.worktree_slug,
                plan.worktree_branch,
                plan.into_branch,
                plan.mode.as_str(),
                plan.commits.len(),
                if plan.commits.is_empty() {
                    "  （无）".to_string()
                } else {
                    plan.commits
                        .iter()
                        .map(|line| format!("  {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                plan.main_dirty_files,
                plan.worktree_dirty_files,
            )));
        }

        let outcome = manager.merge_into(&worktree, mode, false).await?;
        Ok(ToolOutput::ok(format!(
            "已合并 {}（分支 {}）→ {}（mode={}）。\ngit 输出：\n{}\n工作区与分支保留：确认无误后用 ExitWorktree(remove) 清理，或 ExitWorktree(keep) 继续保留。",
            worktree.slug,
            worktree.branch,
            outcome.plan.into_branch,
            outcome.plan.mode.as_str(),
            outcome.output.trim(),
        )))
    }
}
