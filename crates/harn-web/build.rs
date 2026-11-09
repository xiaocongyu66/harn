//! 前端嵌入打包（feature `embed-frontend`）：跑 pnpm 构建 React 工程，
//! 把 dist 拷进 OUT_DIR 并生成清单（路径/MIME/字节），供 dist.rs include。
//!
//! 默认不启用：常规 `cargo build` 无需 pnpm/node；发布构建（或本地验证）
//! 加 `--features harn-web/embed-frontend`。打包环境可用两个 env 逃逸：
//! - `HARN_WEB_FRONTEND_SKIP_INSTALL=1`：跳过 `pnpm install`
//!   （自行保证 node_modules 就绪）；
//! - `HARN_WEB_DIST=<path>`：直接使用预构建好的 dist 目录（完全跳过 pnpm）。
//!
//! 生成物形态（OUT_DIR/frontend_dist.rs）：
//! ```ignore
//! pub static FILES: &[(&str, &str, &[u8])] = &[
//!     ("/", "text/html; charset=utf-8", include_bytes!("…/index.html")),
//!     ...
//! ];
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../harn-web-frontend/src");
    println!("cargo:rerun-if-changed=../harn-web-frontend/index.html");
    println!("cargo:rerun-if-changed=../harn-web-frontend/package.json");
    println!("cargo:rerun-if-changed=../harn-web-frontend/pnpm-lock.yaml");
    println!("cargo:rerun-if-changed=../harn-web-frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=../harn-web-frontend/tsconfig.json");
    if std::env::var("CARGO_FEATURE_EMBED_FRONTEND").is_err() {
        return;
    }
    if let Err(error) = build() {
        // 选了嵌入就必须嵌入成功：给出可操作文案。
        panic!("harn-web: 前端构建失败（{error}）");
    }
}

fn build() -> Result<(), String> {
    let workspace_root = workspace_root().ok_or("无法定位 workspace 根")?;
    let frontend = workspace_root.join("crates/harn-web-frontend");

    // 预构建逃逸：直接用现成 dist。
    if let Ok(dist_env) = std::env::var("HARN_WEB_DIST") {
        let bundle = PathBuf::from(dist_env);
        if bundle.is_dir() {
            return generate_manifest(&bundle).map_err(|e| e.to_string());
        }
        return Err(format!("HARN_WEB_DIST 目录不存在：{}", bundle.display()));
    }

    // 依赖：仅当 node_modules 缺失时安装（打包者可跳过）。
    if !frontend.join("node_modules").is_dir()
        && std::env::var("HARN_WEB_FRONTEND_SKIP_INSTALL").is_err()
    {
        run_command(
            "pnpm",
            &["install", "--frozen-lockfile", "--prefer-offline"],
            &frontend,
        )?;
    }
    run_command("pnpm", &["run", "build"], &frontend)?;

    let bundle = frontend.join("dist");
    if !bundle.is_dir() {
        return Err(format!("找不到前端产物目录：{}", bundle.display()));
    }
    generate_manifest(&bundle).map_err(|e| e.to_string())
}

fn run_command(program: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("启动 {program} 失败：{e}（需 pnpm 与 node 在 PATH）"))?;
    if !status.success() {
        return Err(format!(
            "`{program} {}` 退出码非零（cwd={}）",
            args.join(" "),
            cwd.display()
        ));
    }
    Ok(())
}

/// 收集 dist 文件，生成 OUT_DIR/frontend_dist.rs 清单。
fn generate_manifest(bundle: &Path) -> std::io::Result<()> {
    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR")
            .map_err(|e| std::io::Error::other(format!("OUT_DIR 缺失：{e}")))?,
    );
    let dist_dir = out_dir.join("frontend_dist");
    let _ = fs::remove_dir_all(&dist_dir);
    fs::create_dir_all(&dist_dir)?;

    let mut entries: Vec<(String, &'static str, PathBuf)> = Vec::new();
    collect_files(bundle, bundle, &mut entries)?;
    // 顺序稳定：按路径排序。
    entries.sort();

    let mut generated = String::from(
        "// 由 build.rs 生成；请勿手改。\n/// 内嵌前端清单：(URL 路径, MIME, 字节)。\npub static FILES: &[(&str, &str, &[u8])] = &[\n",
    );
    let total = entries.len();
    for (index, (path, mime, file)) in entries.into_iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let unique = format!("{name}x{index}x{total}");
        let dest = dist_dir.join(&unique);
        fs::copy(&file, &dest)?;
        generated.push_str(&format!(
            "    ({path:?}, {mime:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/frontend_dist/{}\"))),\n",
            unique
        ));
    }
    generated.push_str("];\n");
    fs::write(out_dir.join("frontend_dist.rs"), generated)?;
    Ok(())
}

fn collect_files(
    root: &Path,
    current: &Path,
    entries: &mut Vec<(String, &'static str, PathBuf)>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, entries)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        // URL 路径约定：index.html 同时映射到 `/`。
        let url = format!("/{}", relative.to_string_lossy());
        let mime = mime_for(&path);
        let path = path.clone();
        entries.push((url, mime, path.clone()));
        if relative == Path::new("index.html") {
            entries.push(("/".to_string(), mime, path));
        }
    }
    Ok(())
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn workspace_root() -> Option<PathBuf> {
    // harn-web 在 <root>/crates/harn-web，根即上两级。
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    Some(manifest_dir.parent()?.parent()?.to_path_buf())
}
