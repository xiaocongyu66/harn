//! 工作区管理：项目内 `.harn/worktrees/<slug>` 的 git worktree 生命周期。
//!
//! 解决两个问题：
//! 1. **隔离**——子代理（或主代理自己）的写只落在自己的工作副本，主工作树不受
//!    影响；会话记录不再需要复制到项目外；
//! 2. **可合并**——worktree 与主仓共享 `.git`，合并走 git 本身
//    （cherry-pick / merge），harn 不做自研 diff-apply。
//!
//! 移植 Claude Code `src/utils/worktree.ts` 的核心语义，按 harn 惯例重写：
//! - `git worktree add -B worktree-<slug> <path> <base>`；`-B` 重置残留孤儿分支；
//! - slug 校验（段内 `[a-zA-Z0-9._-]`、拒绝 `.`/`..`、≤64、`/`→`+` 展平）先于
//!   一切副作用；
//! - resume 快路径：`.git` 指针在即复用，不 fetch；
//! - remove 走 fail-closed：有未提交/未合并改动且未显式 discard 则拒绝并列出；
//! - 合并只给 git 命令建议，不自动改用户仓库。

pub mod git;
pub mod tool;

pub use git::{
    MergeHint, MergeMode, MergeOutcome, MergePlan, Worktree, WorktreeError, WorktreeManager,
    WorktreeSummary,
};
pub use tool::{EnterWorktreeTool, ExitWorktreeTool, MergeWorktreeTool};

use std::path::{Path, PathBuf};

/// 会话级当前工作区（Agent 与 EnterWorktree/ExitWorktree 工具间的共享槽；
/// None = 会话仍在主工作树）。
pub type WorktreeSlot = std::sync::Arc<std::sync::Mutex<Option<Worktree>>>;

/// 工作区根目录名（相对项目根）：`项目/.harn/worktrees/<slug>`。
pub fn worktrees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".harn").join("worktrees")
}

/// slug → 分支名：`worktree-<flattened>`。
///
/// `flatten` 把 `/` 换成 `+`：git ref 的 D/F 冲突（`worktree-a` 文件 vs
/// `worktree-a/b` 目录）与目录嵌套（父 worktree 被 remove 连坐删子目录）都被
/// 这个扁平化消除。
pub fn branch_name(slug: &str) -> String {
    format!("worktree-{}", flatten_slug(slug))
}

/// slug → 相对 worktrees 根的目录名（嵌套 slug 展平成单层）。
pub fn worktree_dir_name(slug: &str) -> String {
    flatten_slug(slug)
}

/// 嵌套 slug（`a/b`）→ 单层目录/分支名（`a+b`）。
pub fn flatten_slug(slug: &str) -> String {
    slug.replace('/', "+")
}

/// 随机 slug：形容词-名词-四位随机（对齐 CC CLI 快速路径，如 `swift-fox-a1b2`）。
pub fn random_slug() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const ADJECTIVES: [&str; 12] = [
        "swift", "calm", "bright", "keen", "bold", "quiet", "nimble", "crisp", "fair", "grave",
        "sharp", "vivid",
    ];
    const NOUNS: [&str; 12] = [
        "fox", "otter", "heron", "lynx", "cedar", "ember", "harbor", "summit", "willow", "raven",
        "flint", "orchid",
    ];
    // 进程级计数器 + 时钟熵：不引 rand crate，短周期内也难重复。
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let entropy = nanos.wrapping_mul(6364136223846793005).wrapping_add(seq);
    let adjective = ADJECTIVES[(entropy as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((entropy >> 16) as usize) % NOUNS.len()];
    let suffix = (entropy >> 32) % 36 * 36 + (entropy >> 48) % 36;
    // 36 进制两位
    let mut digits = String::new();
    let mut value = suffix;
    if value == 0 {
        digits.push('0');
    }
    while value > 0 {
        digits.push(char::from_digit((value % 36) as u32, 36).unwrap_or('0'));
        value /= 36;
    }
    format!("{adjective}-{noun}-{digits}")
}

/// slug 合法性（段内 `[a-zA-Z0-9._-]`、拒绝 `.`/`..`、总长 ≤64）。
///
/// 同步校验、先于一切副作用（git 命令/spawn）——路径 join 前的最后防线。
pub fn validate_slug(slug: &str) -> Result<(), WorktreeError> {
    if slug.len() > 64 {
        return Err(WorktreeError::InvalidSlug(format!(
            "工作区名过长（≤64 字符，当前 {}）",
            slug.len()
        )));
    }
    if slug.is_empty() {
        return Err(WorktreeError::InvalidSlug("工作区名为空".into()));
    }
    for segment in slug.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(WorktreeError::InvalidSlug(format!(
                "工作区名 `{slug}` 含空的或 `.`/`..` 路径段"
            )));
        }
        if !segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            return Err(WorktreeError::InvalidSlug(format!(
                "工作区名 `{slug}` 的段 `{segment}` 只能含字母、数字、`.`、`_`、`-`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_slug_accepts_nested_and_flat() {
        assert!(validate_slug("feature-foo").is_ok());
        assert!(validate_slug("asm/feature-foo").is_ok());
        assert!(validate_slug("a.b_c-d").is_ok());
    }

    #[test]
    fn validate_slug_rejects_traversal_and_bad_chars() {
        assert!(validate_slug("..").is_err());
        assert!(validate_slug("../escape").is_err());
        assert!(validate_slug("a//b").is_err());
        assert!(validate_slug("bad$name").is_err());
        assert!(validate_slug("").is_err());
        assert!(validate_slug(&"x".repeat(65)).is_err());
    }

    #[test]
    fn flatten_and_branch_name() {
        assert_eq!(flatten_slug("a/b"), "a+b");
        assert_eq!(branch_name("a/b"), "worktree-a+b");
        assert_eq!(branch_name("feature"), "worktree-feature");
    }

    #[test]
    fn random_slug_shape_and_uniqueness() {
        let a = random_slug();
        let b = random_slug();
        assert!(validate_slug(&a).is_ok(), "{a} 应合法");
        assert!(validate_slug(&b).is_ok(), "{b} 应合法");
        assert_ne!(a, b, "连续两次随机 slug 应不同");
        assert!(a.contains('-'), "{a}");
    }

    #[test]
    fn worktrees_dir_is_inside_project() {
        let root = Path::new("/repo");
        assert_eq!(worktrees_dir(root), PathBuf::from("/repo/.harn/worktrees"));
    }
}
