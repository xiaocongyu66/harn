//! 内嵌前端 dist 的静态供给。
//!
//! - feature `embed-frontend`：build.rs 跑 pnpm 构建前端 dist，生成
//!   OUT_DIR/frontend_dist.rs 的清单（路径/MIME/字节）；
//! - 未启用时回落到占位首页（协议可用但 UI 未嵌）。
//!
//! 路径安全：URL 一律不允许 `..`（直接 404）。

/// 静态文件：MIME + 字节。
pub struct DistFile {
    pub mime: &'static str,
    pub bytes: &'static [u8],
}

/// dist 根下的 index.html 首页占位（未嵌前端时）。
#[cfg(not(feature = "embed-frontend"))]
const PLACEHOLDER_INDEX: &str = include_str!("placeholder.html");

#[cfg(feature = "embed-frontend")]
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/frontend_dist.rs"));
}

pub fn resolve(path: &str) -> Option<DistFile> {
    // 路径安全：只接受 dist 根下的扁平/子路径，禁止任何 `..`。
    if path.contains("..") {
        return None;
    }
    #[cfg(feature = "embed-frontend")]
    {
        // 精确匹配 → 首选；未中再按目录 index.html 兜底。
        if let Some((_, mime, bytes)) = embedded::FILES.iter().find(|(url, _, _)| *url == path) {
            return Some(DistFile { mime, bytes });
        }
        let index = if path.ends_with('/') {
            format!("{path}index.html")
        } else {
            format!("{path}/index.html")
        };
        if let Some((_, mime, bytes)) = embedded::FILES.iter().find(|(url, _, _)| *url == index) {
            return Some(DistFile { mime, bytes });
        }
        None
    }
    #[cfg(not(feature = "embed-frontend"))]
    {
        match path {
            "/" | "/index.html" => Some(DistFile {
                mime: "text/html; charset=utf-8",
                bytes: PLACEHOLDER_INDEX.as_bytes(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_always_serves_index() {
        let file = resolve("/").unwrap();
        assert!(file.bytes.len() > 100, "首页应非空");
    }

    #[test]
    fn traversal_is_blocked() {
        assert!(resolve("/../Cargo.toml").is_none());
        assert!(resolve("/..%2f..%2fetc/passwd").is_none());
    }

    #[test]
    #[cfg(not(feature = "embed-frontend"))]
    fn unknown_paths_404_without_embedding() {
        assert!(resolve("/assets/main.js").is_none());
    }

    #[test]
    #[cfg(feature = "embed-frontend")]
    fn embedded_assets_resolve() {
        // 嵌入 feature 下 JS 资源必须可取；路径带内容哈希，
        // 故按扩展名在清单里找，抗打包器布局变化。
        let js = embedded::FILES
            .iter()
            .find(|(url, _, _)| url.ends_with(".js"));
        let js = js.unwrap_or_else(|| panic!("清单缺 JS：{:?}", embedded::FILES));
        assert_eq!(
            resolve(js.0).unwrap().mime,
            "text/javascript; charset=utf-8"
        );
        assert!(
            embedded::FILES
                .iter()
                .any(|(url, _, _)| url == "/index.html")
        );
        assert!(resolve("/").is_some());
    }
}
