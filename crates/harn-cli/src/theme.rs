//! TUI 主题：默认 CSS 主题 + 用户覆盖（`~/.harn/theme.css`）。
//!
//! 样式由 [ratatui-style](https://crates.io/crates/ratatui-style) 的 CSS 级联引擎
//! 解析——支持选择器优先级、继承、`var()` 令牌与伪状态，最终投影为原生
//! `ratatui::style::Style` / `ratatui::widgets::Block`。

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use harn_config::PermissionMode;
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui_style::{ComputeScratch, ComputedStyle, NodeRef, Origin, Stylesheet};

/// 默认主题：移植 Claude Code 的暗色视觉语言。
///
/// - 高对比语义色：纯白正文、品牌橙、琥珀警告、粉红错误，一眼可辨；
/// - 用户消息色块（`userMessageBackground` 灰底）让「我说的话」从流式文本中跳出；
/// - bash 色块（`bashBorder` 粉勾的灰底），终端原生质感；
/// - 模式徽标随状态变色：plan 青 / accept 紫 / auto 琥珀 / bypass 红——
///   与 CC `getModeColor` 的映射一致（危险即红，而不是统一灰色）。
const DEFAULT_CSS: &str = r#"
:root {
  --bg:        #0e0e10;
  --bg-panel:  #141416;
  --bg-bar:    #1c1c1f;
  --fg:        #ffffff;
  --inactive:  #999999;
  --subtle:    #505050;
  --user-bg:   #373737;
  --bash-bg:   #413c41;
  --claude:    #d77757;
  --suggestion:#b1b9f9;
  --success:   #4eba65;
  --error:     #ff6b80;
  --warning:   #ffc107;
  --plan:      #48968c;
  --accept:    #af87ff;
  --border:    #888888;
  --on-warn:   #11111b;
}

Transcript { color: var(--fg); background: var(--bg-panel); }

Line { color: var(--fg); }
Line.you       { color: var(--fg);        background: var(--user-bg); font-weight: bold; }
Line.assistant { color: var(--fg); }
Line.status    { color: var(--inactive); }
Line.usage     { color: var(--inactive); }
Line.tool      { color: var(--fg); }
Line.ok        { color: var(--success); }
Line.error     { color: var(--error);     font-weight: bold; }
Line.denied    { color: var(--error);     font-weight: bold; }
Line.subagent  { color: var(--suggestion); font-weight: bold; }
Line.plan      { color: var(--plan);      font-weight: bold; }
Line.done      { color: var(--success);   font-weight: bold; }
Line.bash      { color: var(--fg);        background: var(--bash-bg); }
/* Markdown 渲染（移植 CC markdown.ts 的呈现）*/
Line.heading   { color: var(--claude);    font-weight: bold; }
Line.code      { color: var(--fg);        background: var(--bash-bg); }
Line.code-tag  { color: var(--inactive);  font-style: italic; }
Line.quote     { color: var(--inactive);  font-style: italic; }
Line.table     { color: var(--fg); }
Line.link      { color: var(--suggestion); text-decoration: underline; }
/* diff 视图（移植 CC color-diff 的行级底色）*/
Line.diff-add  { color: #a5d6a7; background: #1f3d2a; }
Line.diff-del  { color: #ef9a9a; background: #3d1f24; }
Line.diff-gutter { color: var(--inactive); }

Input { border-style: rounded; border-color: var(--border); }
Input.mode-default  { border-color: var(--border); }
Input.mode-plan     { border-color: var(--plan); }
Input.mode-accept   { border-color: var(--accept); }
Input.mode-auto     { border-color: var(--warning); }
Input.mode-bypass   { border-color: var(--error); }
Input.mode-dontask  { border-color: var(--error); }

Status { color: var(--fg); background: var(--bg-bar); }
Status.busy { color: var(--on-warn); background: var(--warning); font-weight: bold; }

Mode { font-weight: bold; }
Mode.default  { color: var(--fg); }
Mode.plan     { color: var(--plan); }
Mode.accept   { color: var(--accept); }
Mode.auto     { color: var(--warning); }
Mode.bypass   { color: var(--error); }
Mode.dontask  { color: var(--error); }

Modal {
  color: var(--fg);
  background: var(--bg-panel);
  border-style: double;
  border-color: var(--warning);
  padding: 1 1;
}
Modal.permission { color: var(--warning);  border-color: var(--warning); }
Modal.plan       { color: var(--plan);     border-color: var(--plan); }
"#;

/// 静态 UI 元素（输入框、状态栏、弹层）只有有限几种状态，
/// 全部在加载时预计算，绘制时零分配返回原生 `Block` / `Style`。
pub struct Theme {
    /// 仅供转录行按角色 class 惰性计算（角色在运行期才确定）。
    sheet: Stylesheet,
    scratch: RefCell<ComputeScratch>,
    /// `Line` 的继承父级（提供转录正文色）。
    transcript: ComputedStyle,
    status_busy: ComputedStyle,
    status_idle: ComputedStyle,
    modal_permission: ComputedStyle,
    modal_plan: ComputedStyle,
    /// 模式徽标颜色（对齐 CC `getModeColor`）。
    mode_default: ComputedStyle,
    mode_plan: ComputedStyle,
    mode_accept: ComputedStyle,
    mode_auto: ComputedStyle,
    mode_bypass: ComputedStyle,
    mode_dontask: ComputedStyle,
}

/// 模式 → CSS class 后缀（同时用于徽标色与输入框描边色）。
fn mode_class(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::Plan => "plan",
        PermissionMode::AcceptEdits => "accept",
        PermissionMode::Auto => "auto",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::DontAsk => "dontask",
    }
}

/// 级联解析后的颜色字面量 → ratatui 颜色（`Var`/`Inherit` 视为未设置）。
fn paint(color: &ratatui_style::Color) -> ratatui::style::Color {
    match color {
        ratatui_style::Color::Literal(c) => *c,
        ratatui_style::Color::Reset => ratatui::style::Color::Reset,
        ratatui_style::Color::Var { .. } | ratatui_style::Color::Inherit => {
            ratatui::style::Color::Reset
        }
    }
}

impl Theme {
    /// 载入主题：默认 CSS 叠加用户 CSS。
    ///
    /// 用户 CSS 来自 `HARN_THEME_CSS`（文件路径）或 `<HARN_HOME>/theme.css`；
    /// 解析失败时保留默认主题，原因记入 `warnings`。
    pub fn load(warnings: &mut Vec<String>) -> Self {
        let mut sheet = match Stylesheet::parse(DEFAULT_CSS) {
            Ok(sheet) => sheet,
            Err(error) => {
                warnings.push(harn_i18n::tr!(
                    "cli.theme.default_parse_failed",
                    error = error
                ));
                Stylesheet::new()
            }
        };
        if let Some(path) = user_theme_path() {
            merge_user_css(&mut sheet, &path, warnings);
        }
        let mut scratch = ComputeScratch::new();
        let compute = |sheet: &Stylesheet,
                       scratch: &mut ComputeScratch,
                       node: NodeRef<'_>,
                       parent: Option<&ComputedStyle>| {
            sheet.compute_with(&node, parent, scratch)
        };
        let transcript = compute(&sheet, &mut scratch, NodeRef::new("Transcript"), None);
        let status_busy = compute(
            &sheet,
            &mut scratch,
            NodeRef::new("Status").classes(&["busy"]),
            None,
        );
        let status_idle = compute(
            &sheet,
            &mut scratch,
            NodeRef::new("Status").classes(&["idle"]),
            None,
        );
        let modal_permission = compute(
            &sheet,
            &mut scratch,
            NodeRef::new("Modal").classes(&["permission"]),
            None,
        );
        let modal_plan = compute(
            &sheet,
            &mut scratch,
            NodeRef::new("Modal").classes(&["plan"]),
            None,
        );
        let (mode_default, mode_plan, mode_accept, mode_auto, mode_bypass, mode_dontask) = {
            let mut mode_of = |name: &str| {
                compute(
                    &sheet,
                    &mut scratch,
                    NodeRef::new("Mode").classes(&[name]),
                    None,
                )
            };
            (
                mode_of("default"),
                mode_of("plan"),
                mode_of("accept"),
                mode_of("auto"),
                mode_of("bypass"),
                mode_of("dontask"),
            )
        };
        Self {
            sheet,
            scratch: RefCell::new(scratch),
            transcript,
            status_busy,
            status_idle,
            modal_permission,
            modal_plan,
            mode_default,
            mode_plan,
            mode_accept,
            mode_auto,
            mode_bypass,
            mode_dontask,
        }
    }

    /// 转录行样式：`color` 等可继承属性由面板父级流入，class 覆盖角色色。
    pub fn line_style(&self, role: &str) -> Style {
        self.sheet
            .compute_with(
                &NodeRef::new("Line").classes(&[role]),
                Some(&self.transcript),
                &mut self.scratch.borrow_mut(),
            )
            .to_style()
    }

    /// 输入框描边色：跟随当前权限模式（plan 青 / accept 紫 / bypass 红……）。
    pub fn input_border(&self, mode: PermissionMode) -> Style {
        // border 是盒模型属性（`to_block` 才投影）；这里直接取级联后的颜色字面量。
        let computed = self.sheet.compute_with(
            &NodeRef::new("Input").classes(&[&format!("mode-{}", mode_class(mode))]),
            None,
            &mut self.scratch.borrow_mut(),
        );
        match computed
            .style
            .border
            .as_ref()
            .and_then(|b| b.color.as_ref())
        {
            Some(color) => Style::default().fg(paint(color)),
            None => Style::default(),
        }
    }

    /// 模式徽标样式：粗体 + 随模式变化的语义色。
    pub fn mode_style(&self, mode: PermissionMode) -> Style {
        let computed = match mode {
            PermissionMode::Default => &self.mode_default,
            PermissionMode::Plan => &self.mode_plan,
            PermissionMode::AcceptEdits => &self.mode_accept,
            PermissionMode::Auto => &self.mode_auto,
            PermissionMode::BypassPermissions => &self.mode_bypass,
            PermissionMode::DontAsk => &self.mode_dontask,
        };
        computed.to_style()
    }

    /// 转录面板样式（背景色与正文色）。
    pub fn panel_style(&self) -> Style {
        self.transcript.to_style()
    }

    pub fn status(&self, busy: bool) -> Style {
        let computed = if busy {
            &self.status_busy
        } else {
            &self.status_idle
        };
        computed.to_style()
    }

    /// 弹层：`permission = true` 为权限询问，否则为计划审批。
    pub fn modal(&self, permission: bool) -> (Block<'_>, Style) {
        let computed = if permission {
            &self.modal_permission
        } else {
            &self.modal_plan
        };
        (computed.to_block(), computed.to_style())
    }
}

/// 转录行前缀 → CSS class；未识别前缀视为普通助手文本。
pub fn role_class(line: &str) -> &'static str {
    let Some(rest) = line.strip_prefix('[') else {
        return "assistant";
    };
    let Some((tag, _)) = rest.split_once(']') else {
        return "assistant";
    };
    match tag {
        "you" => "user",
        "tool" => "tool",
        "ok" => "ok",
        "error" => "error",
        "denied" => "denied",
        "subagent" => "subagent",
        "plan" => "plan",
        "done" | "interrupt" => "done",
        _ => "assistant",
    }
}

fn user_theme_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("HARN_THEME_CSS")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let path = harn_config::harn_home().join("theme.css");
    path.exists().then_some(path)
}

/// 合并用户 CSS；损坏的用户主题只降级，不影响程序运行。
fn merge_user_css(sheet: &mut Stylesheet, path: &Path, warnings: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        warnings.push(harn_i18n::tr!(
            "cli.theme.read_failed",
            path = path.display()
        ));
        return;
    };
    match Stylesheet::parse_with_origin(&text, Origin::User) {
        Ok(user) => {
            sheet.extend(&user);
        }
        Err(error) => {
            warnings.push(harn_i18n::tr!(
                "cli.theme.parse_failed",
                path = path.display(),
                error = error
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_css_parses_and_has_rules() {
        let sheet = Stylesheet::parse(DEFAULT_CSS).expect("内置主题必须是合法 CSS");
        assert!(sheet.rules().len() > 10);
    }

    #[test]
    fn line_styles_differ_by_role() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        assert!(warnings.is_empty(), "默认主题不应产生告警：{warnings:?}");
        let user = theme.line_style("you");
        let error = theme.line_style("error");
        let assistant = theme.line_style("assistant");
        // 用户行与助手行同正文色，靠底色块区分（对齐 CC userMessageBackground）。
        assert_eq!(user.fg, assistant.fg);
        assert!(user.bg.is_some(), "用户行应有底色块");
        assert!(assistant.bg.is_none(), "助手行不应有底色");
        assert_ne!(error.fg, assistant.fg);
        assert_eq!(error.add_modifier, ratatui::style::Modifier::BOLD);
    }

    #[test]
    fn bash_line_has_block_background() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        let bash = theme.line_style("bash");
        assert!(bash.bg.is_some());
        let ok = theme.line_style("ok");
        assert_eq!(ok.fg, Some(ratatui::style::Color::Rgb(0x4e, 0xba, 0x65)));
    }

    #[test]
    fn mode_styles_change_with_permission_mode() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        let bypass = theme.mode_style(PermissionMode::BypassPermissions);
        let plan = theme.mode_style(PermissionMode::Plan);
        let accept = theme.mode_style(PermissionMode::AcceptEdits);
        let auto = theme.mode_style(PermissionMode::Auto);
        assert_ne!(plan.fg, accept.fg);
        assert_ne!(plan.fg, auto.fg);
        // bypass / dontAsk 都是危险态：同一红色，粗体。
        let dontask = theme.mode_style(PermissionMode::DontAsk);
        assert_eq!(bypass.fg, dontask.fg);
        for style in [bypass, plan, accept, auto, dontask] {
            assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
        }
    }

    #[test]
    fn input_border_follows_mode() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        let default = theme.input_border(PermissionMode::Default);
        let bypass = theme.input_border(PermissionMode::BypassPermissions);
        assert_ne!(default.fg, bypass.fg);
    }

    #[test]
    fn input_and_modal_project_to_native_types() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        assert!(
            theme
                .status(true)
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(
            !theme
                .status(false)
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        let (_permission_block, permission_style) = theme.modal(true);
        let (_plan_block, plan_style) = theme.modal(false);
        assert_ne!(permission_style.fg, plan_style.fg);
        assert!(theme.panel_style().bg.is_some());
    }

    #[test]
    fn all_projections_build_without_panic() {
        let mut warnings = Vec::new();
        let theme = Theme::load(&mut warnings);
        // 只要投影可用即可：Block 可直接交给 Paragraph 渲染。
        let (block, style) = theme.modal(false);
        assert!(!format!("{block:?}").is_empty());
        assert!(style.fg.is_some());
    }

    #[test]
    fn role_class_mapping() {
        assert_eq!(role_class("[you] hi"), "user");
        assert_eq!(role_class("[ok] Read"), "ok");
        assert_eq!(role_class("[denied] Write"), "denied");
        assert_eq!(role_class("[subagent] x"), "subagent");
        assert_eq!(role_class("plain text"), "assistant");
    }

    #[test]
    fn broken_user_theme_keeps_defaults() {
        let _locale_guard = crate::test_support::LOCALE_LOCK.lock().unwrap();
        let mut warnings = Vec::new();
        let dir =
            std::env::temp_dir().join(format!("harn-theme-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let broken = dir.join("theme.css");
        std::fs::write(&broken, "Line.user { color: ; ").unwrap();
        let mut sheet = Stylesheet::parse(DEFAULT_CSS).unwrap();
        let before = sheet.rules().len();
        merge_user_css(&mut sheet, &broken, &mut warnings);
        assert_eq!(sheet.rules().len(), before);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&harn_i18n::tr!("cli.theme.fallback_marker")));
        std::fs::remove_dir_all(dir).ok();
    }
}
