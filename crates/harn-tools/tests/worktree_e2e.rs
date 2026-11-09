//! Worktree 集成测试：tempdir 造真实 git 仓库，走完整生命周期。
//!
//! create → resume → changes → keep → remove（fail-closed 拒绝路径）→
//! discard 删除 → 非 git 仓库报错。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use harn_tools::worktree::{WorktreeError, WorktreeManager};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} 失败");
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    String::from_utf8(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

/// 造一个含一次提交的 git 仓库，返回 (TempDir, repo_root)。
fn git_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().to_path_buf();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "# demo\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    (dir, repo)
}

fn manager(repo: &Path) -> Arc<WorktreeManager> {
    WorktreeManager::new(
        repo.to_path_buf(),
        harn_config::Settings::default().worktree,
    )
}

#[tokio::test]
async fn create_resume_and_changes_full_lifecycle() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);

    // 1) create：目录在项目内 .harn/worktrees/、分支 worktree-<slug>
    let worktree = manager.create("feature-alpha").await.unwrap();
    assert!(worktree.created);
    assert_eq!(worktree.path, repo.join(".harn/worktrees/feature-alpha"));
    assert_eq!(worktree.branch, "worktree-feature-alpha");
    // base 是创建时 HEAD 的绝对 sha（不是字面 "HEAD"：worktree 里的 HEAD 是分支自身）
    assert_ne!(worktree.base, "HEAD");
    assert_eq!(worktree.base.len(), 40, "应为完整 commit sha");
    assert!(worktree.path.join(".git").exists());
    assert!(repo.join("README.md").exists(), "主仓文件不受影响");

    // 2) resume：第二次同 slug 不重复 add（created=false，路径不变）
    let resumed = manager.create("feature-alpha").await.unwrap();
    assert!(!resumed.created);
    assert_eq!(resumed.path, worktree.path);

    // 3) 在 worktree 里制造改动（隔离副本：主仓不受影响）
    std::fs::write(worktree.path.join("src.rs"), "pub fn f() -> u32 { 42 }\n").unwrap();
    let counts = manager.changes(&worktree.path, "HEAD").await.unwrap();
    assert_eq!(counts.files, 1, "一个未跟踪新文件");
    assert_eq!(counts.commits, 0);
    assert!(
        !repo.join("src.rs").exists(),
        "主工作树绝不能看到 worktree 的改动"
    );

    // 4) keep：保留 + 合并建议
    let summary = manager.keep(&worktree).await.unwrap();
    assert_eq!(summary.changes.files, 1);
    let hint = summary.merge_hint.unwrap();
    assert!(
        hint.cherry_pick.contains("cherry-pick")
            && hint.cherry_pick.ends_with("..worktree-feature-alpha")
    );
    assert!(hint.merge.contains("merge worktree-feature-alpha"));

    // 5) remove fail-closed：有改动拒绝
    let error = manager.remove(&worktree, false).await.unwrap_err();
    assert!(
        matches!(error, WorktreeError::RefuseRemove { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("拒绝删除"));
    assert!(worktree.path.exists(), "被拒绝后工作区仍在");

    // 6) discard 删除：worktree + 分支都没了
    manager.remove(&worktree, true).await.unwrap();
    assert!(!worktree.path.exists(), "discard 后工作区删除");
    assert!(
        !git_out(&repo, &["branch", "--list", "worktree-feature-alpha"])
            .contains("worktree-feature-alpha"),
        "分支应删除"
    );
}

#[tokio::test]
async fn clean_worktree_is_auto_removed_on_finish() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("clean-sweep").await.unwrap();
    // 零改动 → finish 自动删除（CC 语义：无残留）
    let summary = manager.finish(&worktree, true).await.unwrap();
    assert_eq!(summary.changes, Default::default());
    assert!(summary.merge_hint.is_none());
    assert!(!worktree.path.exists(), "零改动工作区应自动删除");
}

#[tokio::test]
async fn worktree_with_changes_is_kept_on_finish() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("keep-this").await.unwrap();
    std::fs::write(worktree.path.join("notes.md"), "x").unwrap();
    let summary = manager.finish(&worktree, true).await.unwrap();
    assert_eq!(summary.changes.files, 1);
    assert!(summary.merge_hint.is_some());
    assert!(worktree.path.exists(), "有改动必须保留");
}

#[tokio::test]
async fn create_outside_git_repo_fails_with_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path();
    std::fs::write(plain.join("file.txt"), "x").unwrap();
    let manager = manager(plain);
    let error = manager.create("anything").await.unwrap_err();
    assert!(matches!(error, WorktreeError::NotGitRepo { .. }), "{error}");
    assert!(
        error.to_string().contains("不是 git 仓库"),
        "报错应含可操作信息：{error}"
    );
    assert!(!plain.join(".harn").exists(), "失败不得留下 .harn 目录");
}

#[tokio::test]
async fn list_finds_only_harn_worktrees() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let alpha = manager.create("alpha").await.unwrap();
    let beta = manager.create("beta").await.unwrap();
    let list = manager.list().await.unwrap();
    let slugs: Vec<&str> = list.iter().map(|w| w.slug.as_str()).collect();
    assert_eq!(slugs, vec!["alpha", "beta"]);
    // 外部手工建的 non-harn worktree（repo 内的普通目录 + 普通分支名）不应混入：
    // 既不以 .harn/worktrees/ 为前缀，分支名也无 worktree- 前缀。
    let outside = repo.join("outside-wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual-branch",
            outside.to_str().unwrap(),
            "HEAD",
        ],
    );
    let list = manager.list().await.unwrap();
    assert_eq!(list.len(), 2, "manual 工作区不进列表：{list:?}");
    drop(alpha);
    drop(beta);
}

#[tokio::test]
async fn back_to_back_finish_same_slug() {
    // 第一次 finish 自动删除后，同 slug 再走一轮：create→finish 必须都干净。
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    for round in 1..=3 {
        let worktree = manager.create("cycle").await.unwrap();
        assert!(worktree.created, "第 {round} 轮应新建（上一轮已删）");
        let summary = manager.finish(&worktree, true).await.unwrap();
        assert_eq!(summary.changes, Default::default(), "第 {round} 轮无改动");
        assert!(!summary.merge_hint.is_some());
        assert!(!worktree.path.exists(), "第 {round} 轮结束应自动删除");
    }
    assert!(manager.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn resume_after_finish_with_changes() {
    // 有改动的 finish 保留工作区；同 slug 再次 create 必须 resume（不重建）。
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("kept-feature").await.unwrap();
    std::fs::write(worktree.path.join("work.md"), "x").unwrap();
    let first = manager.finish(&worktree, true).await.unwrap();
    assert!(!first.changes.is_empty());
    assert!(first.merge_hint.is_some());
    let resumed = manager.create("kept-feature").await.unwrap();
    assert!(!resumed.created, "有改动保留后应 resume 而非重建");
    assert_eq!(resumed.path, worktree.path);
    // resume 后再 finish 仍报告同一处改动
    let second = manager.finish(&worktree, true).await.unwrap();
    assert_eq!(second.changes.files, 1);
    assert!(second.merge_hint.is_some());
}

#[tokio::test]
async fn concurrent_shared_slug_finish_is_not_misleading() {
    // 两个"子代理"共享同 slug：先结束者删掉工作区后，后结束者的 finish
    // 不得报告"已保留 + 空合并命令"（误导父代理去合并不存在的东西）。
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let first = manager.create("shared").await.unwrap();
    // 第二个引用同一路径（模拟并发 spawn 的 resume）
    let second = manager.create("shared").await.unwrap();
    assert_eq!(first.path, second.path);
    // 先结束者：零改动 → 自动删除
    let first_summary = manager.finish(&first, true).await.unwrap();
    assert!(!first_summary.merge_hint.is_some());
    assert!(!first.path.exists());
    // 后结束者：工作区已消失 → 空统计、无保留、无合并命令
    let second_summary = manager.finish(&second, true).await.unwrap();
    assert_eq!(second_summary.changes, Default::default());
    assert!(
        second_summary.merge_hint.is_none(),
        "已消失的工作区不得给合并建议：{second_summary:?}"
    );
    assert!(!second.path.exists());
}

#[tokio::test]
async fn merge_cherry_pick_brings_commits_into_main_branch() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("feat").await.unwrap();

    // 在 worktree 里提交一次改动（隔离副本 → 主工作树看不到）
    std::fs::write(worktree.path.join("feature.rs"), "pub fn f() {}\n").unwrap();
    git(&worktree.path, &["add", "."]);
    git(&worktree.path, &["commit", "-q", "-m", "feat: add feature"]);

    // dry-run：列提交、不动主工作树
    let plan = manager
        .merge_plan(&worktree, harn_tools::worktree::MergeMode::CherryPick)
        .await
        .unwrap();
    assert_eq!(plan.commits.len(), 1, "{plan:?}");
    assert!(plan.commits[0].contains("feat: add feature"));
    assert_eq!(plan.main_dirty_files, 0);
    assert!(
        !repo.join("feature.rs").exists(),
        "dry-run 不得改动主工作树"
    );

    // 真合并：cherry-pick 落到主分支
    let outcome = manager
        .merge_into(
            &worktree,
            harn_tools::worktree::MergeMode::CherryPick,
            false,
        )
        .await
        .unwrap();
    assert!(outcome.executed);
    assert!(outcome.conflict.is_none());
    assert!(repo.join("feature.rs").exists(), "合并应把提交带进主工作树");
    assert_eq!(
        git_out(&repo, &["log", "-1", "--format=%s"]).trim(),
        "feat: add feature"
    );
}

#[tokio::test]
async fn merge_conflict_aborts_and_rolls_back() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);

    // 工作区分支改 shared.txt 并提交
    let worktree = manager.create("conflict").await.unwrap();
    std::fs::write(worktree.path.join("shared.txt"), "worktree side\n").unwrap();
    git(&worktree.path, &["add", "."]);
    git(&worktree.path, &["commit", "-q", "-m", "wt: change shared"]);

    // 主工作树改同一行并提交（必然冲突）
    std::fs::write(repo.join("shared.txt"), "main side\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "main: change shared"]);

    let error = manager
        .merge_into(
            &worktree,
            harn_tools::worktree::MergeMode::CherryPick,
            false,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            harn_tools::worktree::WorktreeError::MergeConflict { .. }
        ),
        "{error}"
    );
    // 回退后主工作树干净、无 CONFLICT 残留、内容仍是 main side
    assert!(
        manager.changes(&repo, "HEAD").await.unwrap().files == 0,
        "回退后主工作树必须干净"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("shared.txt")).unwrap(),
        "main side\n",
        "失败合并不得改动主工作树内容"
    );
    assert!(!repo.join(".git").join("MERGE_HEAD").exists());
    assert!(!repo.join(".git").join("CHERRY_PICK_HEAD").exists());
}

#[tokio::test]
async fn merge_refuses_when_main_checkout_is_dirty() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("dirty-main").await.unwrap();
    std::fs::write(worktree.path.join("x.txt"), "x").unwrap();
    git(&worktree.path, &["add", "."]);
    git(&worktree.path, &["commit", "-q", "-m", "wt: add x"]);
    // 主工作树放一个未提交改动
    std::fs::write(repo.join("dirty.txt"), "在途工作").unwrap();

    let error = manager
        .merge_into(
            &worktree,
            harn_tools::worktree::MergeMode::CherryPick,
            false,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("拒绝合并"), "{error}");
    // 在途改动保住
    assert!(repo.join("dirty.txt").exists(), "被拒后用户改动不得丢");
}

#[tokio::test]
async fn merge_with_nothing_committed_is_rejected() {
    let (_dir, repo) = git_repo();
    let manager = manager(&repo);
    let worktree = manager.create("empty-merge").await.unwrap();
    // worktree 有未提交文件但零提交 → 拒绝（提示先 commit 或 keep）
    std::fs::write(worktree.path.join("uncommitted.txt"), "y").unwrap();
    let error = manager
        .merge_into(
            &worktree,
            harn_tools::worktree::MergeMode::CherryPick,
            false,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("没有已提交内容可合并"),
        "{error}"
    );
}
