//! git worktree 生命周期（tokio Command 直行，不经权限 hook）。
//!
//! 所有命令带 `GIT_TERMINAL_PROMPT=0`（防凭据挂起）与显式 cwd。
//! 同步的 slug 校验在调用方（[`super::validate_slug`]）先完成——
//! 本模块的每个入口都假设 slug 已校验（入口处再 validate 一次做纵深防御）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harn_config::WorktreeSettings;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::{branch_name, validate_slug, worktree_dir_name, worktrees_dir};

/// worktree 内记录合并基线（创建时主工作树 commit sha）的文件。
/// 放在 `.harn/` 下：changes() 的 exclude pathspec 已排除它，
/// 不会被 git status 算成用户的未提交改动。
const BASE_FILE: &str = ".harn/worktree-base";

/// 工作区操作错误。
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("不是 git 仓库（{path:?}）：{message}")]
    NotGitRepo { path: PathBuf, message: String },

    #[error("git 命令执行失败：{command}: {message}")]
    GitFailed { command: String, message: String },

    #[error("工作区名非法：{0}")]
    InvalidSlug(String),

    #[error(
        "工作区 {slug:?} 有 {files} 个未提交文件、{commits} 个未合并提交，拒绝删除。\
             显式 discard=true 才销毁（合并或保留请用 action=keep）"
    )]
    RefuseRemove {
        slug: String,
        files: usize,
        commits: usize,
    },

    #[error("找不到工作区 {slug:?}")]
    NotFound { slug: String },

    #[error("合并 `{branch}` → `{into_branch}` 冲突，已回退到合并前状态：\n{detail}")]
    MergeConflict {
        branch: String,
        into_branch: String,
        detail: String,
    },

    #[error("IO 错误：{0}")]
    Io(String),
}

impl From<WorktreeError> for harn_core::error::HarnError {
    fn from(value: WorktreeError) -> Self {
        harn_core::error::HarnError::Tool(value.to_string())
    }
}

/// 一个已创建/已接管的工作区记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// 用户/模型给的 slug（未嵌套展平前的原样）。
    pub slug: String,
    /// 绝对路径：`<repo>/.harn/worktrees/<flattened>`。
    pub path: PathBuf,
    /// 分支名：`worktree-<flattened>`。
    pub branch: String,
    /// 创建时的基线分支（`HEAD`/`origin/main`/…）。
    pub base: String,
    /// 本次调用是新建还是复用。
    pub created: bool,
}

/// 工作区变更统计（`git status --porcelain` 行数 + `rev-list` 提交计数）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub files: usize,
    pub commits: usize,
}

impl ChangeCounts {
    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.commits == 0
    }
}

/// 给用户的合并建议（cherry-pick / merge 两条命令，附分支与路径）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeHint {
    pub worktree_path: PathBuf,
    pub branch: String,
    /// `git -C <repo> cherry-pick <base>..<branch>`
    pub cherry_pick: String,
    /// `git -C <repo> merge <branch>`
    pub merge: String,
}

/// 合并模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMode {
    /// 线性历史：`cherry-pick base..branch`（默认）。
    CherryPick,
    /// 保留分支拓扑：`merge --no-ff`（生成合并提交）。
    Merge,
}

impl MergeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick",
            Self::Merge => "merge",
        }
    }
}

/// 合并预演（dry-run 的返回）。
#[derive(Clone, Debug)]
pub struct MergePlan {
    pub worktree_slug: String,
    pub worktree_branch: String,
    pub into_branch: String,
    pub mode: MergeMode,
    /// `git log base..branch` 的 `%h %s` 行（空 = 没有已提交内容）。
    pub commits: Vec<String>,
    pub main_dirty_files: usize,
    pub worktree_dirty_files: usize,
}

/// 合并执行结果。
#[derive(Clone, Debug)]
pub struct MergeOutcome {
    pub plan: MergePlan,
    /// `false` = dry-run（未执行）。
    pub executed: bool,
    /// 冲突回退的 git 输出（None = 无冲突）。
    pub conflict: Option<String>,
    /// git stdout/stderr 摘录。
    pub output: String,
}

/// 子代理/会话结束时回传给上层的摘要。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeSummary {
    pub slug: String,
    pub path: PathBuf,
    pub branch: String,
    pub changes: ChangeCounts,
    /// 有改动时的合并建议；无改动为 None（此时工作区应已被删）。
    pub merge_hint: Option<MergeHint>,
}

/// 工作区管理器（进程级共享）。
pub struct WorktreeManager {
    repo_root: PathBuf,
    settings: WorktreeSettings,
    /// 同 slug 的 create/remove 串行化（Tokio Mutex；不跨 await 持锁做多步 IO）。
    lock: Mutex<()>,
}

impl WorktreeManager {
    /// 构造（`cwd` 处的 git 仓库根在首次使用时解析）。
    pub fn new(cwd: PathBuf, settings: WorktreeSettings) -> Arc<Self> {
        Arc::new(Self {
            repo_root: cwd,
            settings,
            lock: Mutex::new(()),
        })
    }

    /// 从设置告警（启动时打印）。
    pub fn settings_warnings(&self) -> Vec<String> {
        self.settings.warnings()
    }

    /// 解析（并缓存）git 仓库根；非 git 仓库报错。
    pub async fn git_root(&self) -> Result<PathBuf, WorktreeError> {
        let output = git_command(&self.repo_root)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::NotGitRepo {
                path: self.repo_root.clone(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if root.is_empty() {
            return Err(WorktreeError::NotGitRepo {
                path: self.repo_root.clone(),
                message: "rev-parse 输出为空".into(),
            });
        }
        Ok(PathBuf::from(root))
    }

    /// 创建或复用（resume 快路径：`.git` 指针在即复用，不 fetch/add）。
    pub async fn create(&self, slug: &str) -> Result<Worktree, WorktreeError> {
        validate_slug(slug)?;
        let guard = self.lock.lock().await;
        let root = self.git_root().await?;
        let path = worktrees_dir(&root).join(worktree_dir_name(slug));
        let branch = branch_name(slug);

        // resume：worktree 目录已注册（.git 指针文件存在）。
        // base 从持久化文件读（HEAD 是 worktree 分支自身，不能直接 rev-parse）。
        if path.join(".git").exists() {
            let base = match tokio::fs::read_to_string(path.join(BASE_FILE)).await {
                Ok(text) => text.trim().to_string(),
                Err(_) => {
                    // 文件丢了（用户手删/旧 worktree）：降级到主工作树 HEAD，
                    // 合并范围按“主树现状”计（可 diff 出来核对，不会更激进）。
                    self.resolve_commit(&root, "HEAD")
                        .await
                        .unwrap_or_else(|_| "HEAD".to_string())
                }
            };
            return Ok(Worktree {
                slug: slug.to_string(),
                path,
                branch,
                base,
                created: false,
            });
        }

        let base_ref = self.base_ref().await?;
        tokio::fs::create_dir_all(worktrees_dir(&root))
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;

        // base 可解析性检查（settings 给了不存在的分支时明确报错，不进 -B）；
        // 之后一律用**主工作树上的 commit sha**（不是 "HEAD"：worktree 里的
        // HEAD 是本分支自身，`HEAD..branch` 会恒为空）。
        let base = self.resolve_commit(&root, &base_ref).await?;

        let mut args = vec!["worktree".to_string(), "add".to_string()];
        if !self.settings.sparse_paths.is_empty() {
            args.push("--no-checkout".to_string());
        }
        args.push("-B".to_string());
        args.push(branch.clone());
        args.push(path.display().to_string());
        args.push(base.clone());
        let output = git_command(&root)
            .args(&args)
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !output.status.success() {
            // 半成品拆掉：--no-checkout 失败时 worktree 已注册但工作树为空，
            // 下次 resume 会把坏工作区当好的呈现（对齐 CC tearDown）。
            let _ = git_command(&root)
                .args(["worktree", "remove", "--force", &path.display().to_string()])
                .output()
                .await;
            return Err(WorktreeError::GitFailed {
                command: "git worktree add".into(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        if !self.settings.sparse_paths.is_empty() {
            let sparse = git_command(&path)
                .args(["sparse-checkout", "set", "--cone"])
                .args(&self.settings.sparse_paths)
                .output()
                .await
                .map_err(|error| WorktreeError::Io(error.to_string()))?;
            if !sparse.status.success() {
                let _ = git_command(&root)
                    .args(["worktree", "remove", "--force", &path.display().to_string()])
                    .output()
                    .await;
                return Err(WorktreeError::GitFailed {
                    command: "git sparse-checkout set".into(),
                    message: String::from_utf8_lossy(&sparse.stderr).trim().to_string(),
                });
            }
        }

        // base 落盘（resume/merge 都要用；在 worktree 里 rev-parse HEAD 拿到的是自己）
        if let Some(parent) = path.join(BASE_FILE).parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(path.join(BASE_FILE), &base).await;
        // 主仓 info/exclude 排除 worktree 根：否则用户 `git add .` 会把
        // `.harn/worktrees/<slug>`（内部是 git worktree，含 .git 指针）
        // 当嵌入式仓库加进主仓历史。
        self.exclude_in_main_repo(&root).await;
        self.symlink_directories(&root, &path).await;

        drop(guard);
        Ok(Worktree {
            slug: slug.to_string(),
            path,
            branch,
            base,
            created: true,
        })
    }

    /// 列出全部 worktree（`git worktree list --porcelain`）。
    pub async fn list(&self) -> Result<Vec<Worktree>, WorktreeError> {
        let root = self.git_root().await?;
        let output = git_command(&root)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::GitFailed {
                command: "git worktree list".into(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;
        let mut flush = |path: Option<PathBuf>, branch: Option<String>| {
            if let Some(path) = path
                && let Some(branch) = branch
                && let Some(slug) = slug_from_branch(&branch)
                && path.starts_with(worktrees_dir(&root))
            {
                out.push(Worktree {
                    slug,
                    path,
                    branch,
                    base: String::new(),
                    created: false,
                });
            }
        };
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                flush(current_path.take(), current_branch.take());
                current_path = Some(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("branch ") {
                current_branch = Some(rest.trim_start_matches("refs/heads/").to_string());
            } else if line.starts_with("detached") {
                current_branch = None;
            }
        }
        flush(current_path, current_branch);
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    /// 变更统计：`git status --porcelain` 行数 + `rev-list --count base..HEAD`。
    pub async fn changes(
        &self,
        worktree_path: &Path,
        base: &str,
    ) -> Result<ChangeCounts, WorktreeError> {
        // `.harn` 是 harness 自管目录（worktree/会话记录都在里面），
        // 不计入用户的在途改动：否则 merge 的“主树脏”判定会被自己建的
        // worktree 绊倒（未排除时 root 的 status 永远 ≥1）。
        let status = git_command(worktree_path)
            .args([
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--",
                ".",
                ":(exclude).harn",
            ])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !status.status.success() {
            return Err(WorktreeError::GitFailed {
                command: "git status".into(),
                message: String::from_utf8_lossy(&status.stderr).trim().to_string(),
            });
        }
        let files = String::from_utf8_lossy(&status.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        // base 不可解析时（worktree 被 reset 过）提交数未知 → 按有改动处理（fail-closed）
        let commits = match self.resolve_commit(worktree_path, base).await {
            Ok(resolved) => {
                let rev = git_command(worktree_path)
                    .args(["rev-list", "--count", &format!("{resolved}..HEAD")])
                    .output()
                    .await
                    .map_err(|error| WorktreeError::Io(error.to_string()))?;
                if rev.status.success() {
                    String::from_utf8_lossy(&rev.stdout)
                        .trim()
                        .parse::<usize>()
                        .unwrap_or(0)
                } else {
                    1
                }
            }
            Err(_) => 1,
        };
        Ok(ChangeCounts { files, commits })
    }

    /// 保留工作区（什么都不删，只报告）。
    pub async fn keep(&self, worktree: &Worktree) -> Result<WorktreeSummary, WorktreeError> {
        Ok(self.summarize(worktree).await)
    }

    /// 删除工作区 + 分支。**fail-closed**：有改动且未 discard 时拒绝。
    pub async fn remove(&self, worktree: &Worktree, discard: bool) -> Result<(), WorktreeError> {
        validate_slug(&worktree.slug)?;
        let guard = self.lock.lock().await;
        let root = self.git_root().await?;
        let changes = self
            .changes(&worktree.path, &self.base_ref().await?)
            .await?;
        if !changes.is_empty() && !discard {
            return Err(WorktreeError::RefuseRemove {
                slug: worktree.slug.clone(),
                files: changes.files,
                commits: changes.commits,
            });
        }
        let remove = git_command(&root)
            .args([
                "worktree",
                "remove",
                "--force",
                &worktree.path.display().to_string(),
            ])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !remove.status.success() {
            return Err(WorktreeError::GitFailed {
                command: "git worktree remove".into(),
                message: String::from_utf8_lossy(&remove.stderr).trim().to_string(),
            });
        }
        // 分支删除失败（不存在/被 checkout）不阻断工作区删除：记录但不报错。
        let _ = git_command(&root)
            .args(["branch", "-D", &worktree.branch])
            .output()
            .await;
        drop(guard);
        Ok(())
    }

    /// 收尾：无改动 → 自动删除；有改动 → 保留并附合并建议。
    ///
    /// 工作区已消失（并发共享 slug 时先结束者已删）不算错误：
    /// 返回空统计，调用方据此认为"没有待合并内容"。
    pub async fn finish(
        &self,
        worktree: &Worktree,
        auto_remove_when_clean: bool,
    ) -> Result<WorktreeSummary, WorktreeError> {
        if !worktree.path.exists() {
            return Ok(WorktreeSummary {
                slug: worktree.slug.clone(),
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                changes: ChangeCounts::default(),
                merge_hint: None,
            });
        }
        let counts = self.changes(&worktree.path, &worktree.base).await?;
        if counts.is_empty() && auto_remove_when_clean {
            let root = self.git_root().await?;
            let _ = git_command(&root)
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &worktree.path.display().to_string(),
                ])
                .output()
                .await;
            let _ = git_command(&root)
                .args(["branch", "-D", &worktree.branch])
                .output()
                .await;
            return Ok(WorktreeSummary {
                slug: worktree.slug.clone(),
                path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                changes: counts,
                merge_hint: None,
            });
        }
        Ok(WorktreeSummary {
            slug: worktree.slug.clone(),
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            changes: counts,
            merge_hint: Some(self.merge_hint(worktree).await),
        })
    }

    /// 合并命令建议（不执行）。
    pub async fn merge_hint(&self, worktree: &Worktree) -> MergeHint {
        let root = self
            .git_root()
            .await
            .unwrap_or_else(|_| self.repo_root.clone());
        MergeHint {
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            cherry_pick: format!(
                "git -C {} cherry-pick {}..{}",
                root.display(),
                worktree.base,
                worktree.branch
            ),
            merge: format!("git -C {} merge {}", root.display(), worktree.branch),
        }
    }

    /// 主工作树当前分支（detached HEAD → 错误：合并目标必须是命名分支）。
    pub async fn main_branch(&self) -> Result<String, WorktreeError> {
        let root = self.git_root().await?;
        let output = git_command(&root)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if branch.is_empty() {
                return Err(WorktreeError::GitFailed {
                    command: "git symbolic-ref HEAD".into(),
                    message: "主工作树处于 detached HEAD：请先 checkout 一个分支".into(),
                });
            }
            Ok(branch)
        } else {
            Err(WorktreeError::GitFailed {
                command: "git symbolic-ref HEAD".into(),
                message: format!(
                    "主工作树处于 detached HEAD 或无提交，不能作为合并目标：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }

    /// 合并预演：列出将进入的提交 + 两侧是否干净（不碰任何状态）。
    pub async fn merge_plan(
        &self,
        worktree: &Worktree,
        mode: MergeMode,
    ) -> Result<MergePlan, WorktreeError> {
        let root = self.git_root().await?;
        let into_branch = self.main_branch().await?;
        if into_branch == worktree.branch {
            return Err(WorktreeError::GitFailed {
                command: "MergeWorktree".into(),
                message: format!("主工作树就在分支 `{into_branch}` 上，不能合并进自身"),
            });
        }
        let log = git_command(&worktree.path)
            .args([
                "log",
                "--format=%h %s",
                &format!("{}..{}", worktree.base, worktree.branch),
            ])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if !log.status.success() {
            return Err(WorktreeError::GitFailed {
                command: "git log base..branch".into(),
                message: String::from_utf8_lossy(&log.stderr).trim().to_string(),
            });
        }
        let commits: Vec<String> = String::from_utf8_lossy(&log.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        // 主工作树必须干净：cherry-pick/merge 都需要可写的暂存区
        let main_counts = self.changes(&root, &worktree.base).await?;
        let branch_counts = self.changes(&worktree.path, &worktree.base).await?;
        Ok(MergePlan {
            worktree_slug: worktree.slug.clone(),
            worktree_branch: worktree.branch.clone(),
            into_branch,
            mode,
            commits,
            main_dirty_files: main_counts.files,
            worktree_dirty_files: branch_counts.files,
        })
    }

    /// 执行合并（只进主工作树当前分支；冲突即整体回退，不留半合并状态）。
    ///
    /// `dry_run` 只返回计划不执行。未提交的 worktree 改动不会被带入——
    /// 合并的是已提交内容（未提交部分仍留在 worktree，可用 ExitWorktree 查看）。
    pub async fn merge_into(
        &self,
        worktree: &Worktree,
        mode: MergeMode,
        dry_run: bool,
    ) -> Result<MergeOutcome, WorktreeError> {
        let plan = self.merge_plan(worktree, mode).await?;
        if dry_run {
            return Ok(MergeOutcome {
                plan,
                executed: false,
                conflict: None,
                output: String::new(),
            });
        }
        if plan.commits.is_empty() {
            return Err(WorktreeError::GitFailed {
                command: "MergeWorktree".into(),
                message: format!(
                    "工作区 `{}` 没有已提交内容可合并（{} 个未提交文件不会带入：先在 worktree 里 commit，或 ExitWorktree keep 查看）",
                    worktree.slug, plan.worktree_dirty_files
                ),
            });
        }
        if plan.main_dirty_files > 0 {
            return Err(WorktreeError::GitFailed {
                command: "MergeWorktree".into(),
                message: format!(
                    "主工作树有 {} 个未提交改动，拒绝合并（请先提交或 stash，避免把你的在途工作卷进合并）",
                    plan.main_dirty_files
                ),
            });
        }
        let root = self.git_root().await?;
        let outcome = match mode {
            MergeMode::CherryPick => {
                let commitish = format!("{}..{}", worktree.base, worktree.branch);
                let output = git_command(&root)
                    .args(["cherry-pick", &commitish])
                    .output()
                    .await
                    .map_err(|error| WorktreeError::Io(error.to_string()))?;
                (vec!["cherry-pick"], output)
            }
            MergeMode::Merge => {
                let output = git_command(&root)
                    .args(["merge", "--no-ff", &worktree.branch])
                    .output()
                    .await
                    .map_err(|error| WorktreeError::Io(error.to_string()))?;
                (vec!["merge", "--no-ff"], output)
            }
        };
        let (command_parts, output) = outcome;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            return Ok(MergeOutcome {
                plan,
                executed: true,
                conflict: None,
                output: if stdout.trim().is_empty() {
                    stderr
                } else {
                    stdout
                },
            });
        }
        // 失败 → 判定是否冲突 → 回退（cherry-pick --abort / merge --abort），
        // 恢复主工作树到合并前状态（fail-closed：不留半合并状态）。
        // 判定不依赖 git 的英文输出（locale 可能是中文）：用 merge/cherry-pick
        // 的状态文件存在与否——它们由 git 自己写，与语言无关。
        let text = format!("{stdout}{stderr}");
        let state_file = "CHERRY_PICK_HEAD";
        let conflicted = root.join(".git").join(state_file).exists()
            || root.join(".git").join("MERGE_HEAD").exists()
            || text.contains("CONFLICT")
            || text.contains("conflict");
        if conflicted {
            let abort_arg = match mode {
                MergeMode::CherryPick => "cherry-pick",
                MergeMode::Merge => "merge",
            };
            let _ = git_command(&root)
                .args([abort_arg, "--abort"])
                .output()
                .await;
            return Err(WorktreeError::MergeConflict {
                branch: worktree.branch.clone(),
                into_branch: plan.into_branch.clone(),
                detail: text.trim().to_string(),
            });
        }
        Err(WorktreeError::GitFailed {
            command: command_parts.join(" "),
            message: text.trim().to_string(),
        })
    }

    async fn summarize(&self, worktree: &Worktree) -> WorktreeSummary {
        let changes = self
            .changes(&worktree.path, &worktree.base)
            .await
            .unwrap_or_default();
        let merge_hint = if changes.is_empty() {
            None
        } else {
            Some(self.merge_hint(worktree).await)
        };
        WorktreeSummary {
            slug: worktree.slug.clone(),
            path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            changes,
            merge_hint,
        }
    }

    /// 基线 ref：settings.baseBranch 已解析则用，否则 HEAD。
    async fn base_ref(&self) -> Result<String, WorktreeError> {
        match &self.settings.base_branch {
            Some(branch) if !branch.trim().is_empty() => Ok(branch.trim().to_string()),
            _ => Ok("HEAD".to_string()),
        }
    }

    /// 幂等地把 `.harn/worktrees/` 写进主仓 `info/exclude`（仓库级排除，
    /// 不碰用户 .gitignore；写失败只是体验降级，不阻断）。
    async fn exclude_in_main_repo(&self, root: &Path) {
        let exclude_path = git_command(root)
            .args(["rev-parse", "--git-path", "info/exclude"])
            .output()
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|path| !path.is_empty());
        let Some(exclude_path) = exclude_path else {
            return;
        };
        let exclude_path = PathBuf::from(exclude_path);
        let exclude_path = if exclude_path.is_absolute() {
            exclude_path
        } else {
            root.join(exclude_path)
        };
        let existing = tokio::fs::read_to_string(&exclude_path)
            .await
            .unwrap_or_default();
        if existing
            .lines()
            .any(|line| line.trim() == ".harn/worktrees/")
        {
            return;
        }
        if let Some(parent) = exclude_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(".harn/worktrees/\n");
        let _ = tokio::fs::write(&exclude_path, content).await;
    }

    /// 把 ref（HEAD / origin/main / sha）解析成**主工作树上的绝对 commit sha**。
    ///
    /// 必须在 `root`（主工作树）上解析：worktree 里的 HEAD 是其分支自身。
    async fn resolve_commit(&self, root: &Path, base: &str) -> Result<String, WorktreeError> {
        let output = git_command(root)
            .args(["rev-parse", "--verify", base])
            .output()
            .await
            .map_err(|error| WorktreeError::Io(error.to_string()))?;
        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if sha.is_empty() {
                return Err(WorktreeError::GitFailed {
                    command: format!("git rev-parse --verify {base}"),
                    message: format!("基线 `{base}` 解析结果为空（仓库可能没有任何提交）"),
                });
            }
            Ok(sha)
        } else {
            Err(WorktreeError::GitFailed {
                command: format!("git rev-parse --verify {base}"),
                message: format!("基线 `{base}` 不存在，请检查 settings worktree.baseBranch"),
            })
        }
    }

    /// 把主仓的 symlinkDirectories 链进工作区（防 node_modules 复制膨胀）。
    async fn symlink_directories(&self, root: &Path, worktree: &Path) {
        for directory in &self.settings.symlink_directories {
            if directory.split(['/', '\\']).any(|segment| segment == "..") {
                continue; // 已在 settings warnings 里告警；这里静默跳过
            }
            let source = root.join(directory);
            let target = worktree.join(directory);
            if !source.is_dir() || target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::symlink(&source, &target).await;
        }
    }
}

/// git 命令构造：禁终端提示（凭据挂起是 LSP/CLI 挂死的经典原因）。
fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// 分支名反查 slug（`worktree-<slug>` → `<slug>`；+ 还原为 / 无意义，目录名已扁平）。
fn slug_from_branch(branch: &str) -> Option<String> {
    branch.strip_prefix("worktree-").map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_in(dir: &Path) -> Arc<WorktreeManager> {
        WorktreeManager::new(dir.to_path_buf(), WorktreeSettings::default())
    }

    #[test]
    fn slug_from_branch_roundtrip() {
        assert_eq!(
            slug_from_branch("worktree-feature-foo").as_deref(),
            Some("feature-foo")
        );
        assert_eq!(
            slug_from_branch("worktree-asm+feature").as_deref(),
            Some("asm+feature")
        );
        assert_eq!(slug_from_branch("main"), None);
        assert_eq!(slug_from_branch("worktree-").as_deref(), Some(""));
    }

    #[tokio::test]
    async fn git_root_fails_outside_repo_with_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager_in(dir.path());
        let error = manager.git_root().await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("不是 git 仓库"), "{message}");
    }

    #[tokio::test]
    async fn merge_hint_reads_like_git_commands() {
        // 不在 git 仓也能产出建议文案（root 回退构造目录）
        let dir = tempfile::tempdir().unwrap();
        let manager = manager_in(dir.path());
        let worktree = Worktree {
            slug: "feature".into(),
            path: dir.path().join(".harn/worktrees/feature"),
            branch: "worktree-feature".into(),
            base: "HEAD".into(),
            created: true,
        };
        let hint = manager.merge_hint(&worktree).await;
        assert!(
            hint.cherry_pick.starts_with("git -C "),
            "{}",
            hint.cherry_pick
        );
        assert!(
            hint.cherry_pick.ends_with("HEAD..worktree-feature"),
            "{}",
            hint.cherry_pick
        );
        assert!(
            hint.merge.ends_with("merge worktree-feature"),
            "{}",
            hint.merge
        );
        assert_eq!(hint.branch, "worktree-feature");
    }

    #[tokio::test]
    async fn create_rejects_invalid_slug_before_touching_git() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager_in(dir.path());
        for bad in ["../escape", "a//b", "", &"x".repeat(65)] {
            let error = manager.create(bad).await.unwrap_err();
            assert!(
                matches!(error, WorktreeError::InvalidSlug(_)),
                "{bad}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn changes_reports_zero_in_clean_repo() {
        // 真实 git：init + commit + 在同一仓库无改动 → files=0, commits=0
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .status()
            .unwrap();
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .status()
            .unwrap();
        for (key, value) in [("user.email", "t@t"), ("user.name", "t")] {
            std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(repo)
                .status()
                .unwrap();
        }
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(repo)
            .status()
            .unwrap();
        let manager = manager_in(repo);
        let counts = manager.changes(repo, "HEAD").await.unwrap();
        assert_eq!(
            counts,
            ChangeCounts {
                files: 0,
                commits: 0
            }
        );
    }
}
