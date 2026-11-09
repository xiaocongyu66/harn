//! 界面文案的本地化基础设施（rust-i18n 引擎 + 统一顶层 `locales/` 目录）。
//!
//! - 语言包：**项目根 `locales/{zh,en}.json`**（不散落在 crate 里，集中维护）；
//!   语义英文 key（如 `cmd.exit.description`），`{name}` 占位符参数化。
//! - 运行时加载（非编译期 embed）：`SimpleBackend` + 目录解析
//!   `HARN_LOCALES_DIR` > 可执行文件相对 > 源码树相对，新增语言免重编。
//! - 默认中文；`settings.locale` 覆盖，其次 `LANG` 等环境变量，兜底 `zh`。
//! - 缺 key 返回 key 本身（`missing_keys` 测试兜底检测 zh/en 差异）。

use std::collections::HashMap;
use std::path::PathBuf;

use once_cell::sync::Lazy;
use rust_i18n::SimpleBackend;
use rust_i18n_support::{Backend, load_locales};

/// 兜底语言：任何查找失败都回退中文。
const FALLBACK_LOCALE: &str = "zh";

/// 语言包目录：显式环境变量 > 可执行文件相对（安装布局）> 源码树相对（开发/测试）。
fn locales_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("HARN_LOCALES_DIR")
        && !dir.trim().is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(base) = exe.parent()
    {
        for relative in ["../locales", "../../locales"] {
            let candidate = base.join(relative);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    // 开发树：crate 位于 crates/harn-i18n，语言包在仓库根。
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("../../locales");
    if candidate.is_dir() {
        return Some(candidate);
    }
    None
}

/// 全局翻译后端（首次查询时惰性加载目录；无目录时退化为“缺 key 回退 key”）。
static BACKEND: Lazy<SimpleBackend> = Lazy::new(|| {
    let mut backend = SimpleBackend::new();
    if let Some(dir) = locales_dir() {
        for (locale, translations) in load_locales(&dir.to_string_lossy(), |_| false) {
            let borrowed: HashMap<&str, &str> = translations
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            backend.add_translations(&locale, &borrowed);
        }
    }
    backend
});

/// 当前 locale（`zh` / `en`）。
pub fn current_locale() -> String {
    rust_i18n::locale().to_string()
}

/// 切换 locale（运行时全局生效）。
pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

/// 后端已加载的语言列表（按字母序）。
pub fn available_locales() -> Vec<String> {
    let mut locales: Vec<String> = BACKEND
        .available_locales()
        .iter()
        .map(|locale| (*locale).to_string())
        .collect();
    locales.sort();
    locales
}

/// 计算生效 locale：显式配置优先，其次环境变量（`HARN_LOCALE`/`LC_ALL`/
/// `LC_MESSAGES`/`LANG` 的语言段），都不命中回退中文。
pub fn resolve_locale(explicit: Option<&str>) -> String {
    if let Some(locale) = explicit.filter(|value| !value.trim().is_empty()) {
        return normalize(locale);
    }
    for name in ["HARN_LOCALE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return normalize(&value);
        }
    }
    FALLBACK_LOCALE.to_string()
}

/// `zh-CN` / `zh_CN.UTF-8` → `zh`；仅保留已支持的语言，其余回退中文。
fn normalize(raw: &str) -> String {
    let language = raw
        .split(['-', '_', '.'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match language.as_str() {
        "zh" | "en" => language,
        _ => FALLBACK_LOCALE.to_string(),
    }
}

/// 翻译入口（供 `tr!` 宏与测试调用）：当前 locale → 兜底中文 → key 本身，
/// 随后替换 `{name}` 占位符。
pub fn translate(key: &str, args: &[(&str, String)]) -> String {
    let locale = current_locale();
    let template = BACKEND
        .translate(&locale, key)
        .or_else(|| BACKEND.translate(FALLBACK_LOCALE, key))
        .unwrap_or(key);
    if args.is_empty() {
        return template.to_string();
    }
    let mut out = template.to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// 翻译宏：`tr!("cmd.exit.description")` /
/// `tr!("tool.write.created", bytes = n, path = p)`。
///
/// 占位符与语言包里的 `{name}` 对应；参数在调用点求值。
#[macro_export]
macro_rules! tr {
    ($key:expr $(,)?) => {
        $crate::translate($key, &[])
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::translate(
            $key,
            &[$( (stringify!($name), $value.to_string()) ),*],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn locales_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../locales")
    }

    fn locale_keys(locale: &str) -> BTreeSet<String> {
        let path = locales_root().join(format!("{locale}.json"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {path:?}: {e}"));
        let value: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("解析 {path:?}: {e}"));
        value
            .as_object()
            .expect("顶层应为对象")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn locale_files_have_identical_key_sets() {
        let zh = locale_keys("zh");
        let en = locale_keys("en");
        assert_eq!(zh, en, "zh/en 语言包 key 集合不一致");
    }

    #[test]
    fn backend_loads_unified_locales_directory() {
        let locales = available_locales();
        assert!(
            locales.iter().any(|locale| locale == "zh"),
            "应加载 zh：{locales:?}"
        );
        assert!(
            locales.iter().any(|locale| locale == "en"),
            "应加载 en：{locales:?}"
        );
    }

    #[test]
    fn resolve_locale_prefers_explicit_then_lang() {
        assert_eq!(resolve_locale(Some("en")), "en");
        assert_eq!(resolve_locale(Some("zh-CN")), "zh");
        assert_eq!(resolve_locale(Some("")), resolve_locale(None));
        assert_eq!(normalize("en_US.UTF-8"), "en");
        assert_eq!(normalize("fr"), "zh", "不支持的语言回退中文");
    }

    #[test]
    fn translate_looks_up_zh_and_en_with_fallback() {
        let original = current_locale();
        set_locale("zh");
        assert_eq!(translate("cmd.exit.description", &[]), "退出");
        set_locale("en");
        assert_eq!(translate("cmd.exit.description", &[]), "Quit");
        assert_eq!(
            translate("no.such.key", &[]),
            "no.such.key",
            "缺 key 回退 key 本身"
        );
        set_locale("fr");
        assert_eq!(
            translate("cmd.exit.description", &[]),
            "退出",
            "未知 locale 回退中文"
        );
        set_locale(&original);
    }

    #[test]
    fn tr_macro_interpolates_placeholders() {
        let out = tr!("test.interpolated", name = "harn", count = 3usize);
        assert_eq!(out, "harn 3");
    }
}
