//! LSP 结果的文本格式化（对齐 CC `LSPTool/formatters.ts` 的输出结构）。
//!
//! 位置统一展示为 1-based 的 `path:line:col`；引用/符号按文件分组；
//! 空结果给出「可能原因」提示，而不是裸 null。

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, DocumentSymbol, GotoDefinitionResponse,
    Hover, Location, LocationLink, SymbolInformation,
};

/// URI → 展示路径：相对 cwd 且不比绝对路径更长时用相对路径（对齐 CC formatUri）。
pub fn display_path(uri_str: &str, cwd: &std::path::Path) -> String {
    let Ok(uri) = url::Url::parse(uri_str) else {
        return uri_str.to_string();
    };
    let Ok(absolute) = uri.to_file_path() else {
        return uri_str.to_string();
    };
    if let Ok(relative) = absolute.strip_prefix(cwd) {
        let relative = relative.display().to_string().replace('\\', "/");
        if !relative.is_empty() {
            return relative;
        }
    }
    absolute.display().to_string().replace('\\', "/")
}

/// `path:line:character`（1-based）。
pub fn format_location(location: &Location, cwd: &std::path::Path) -> String {
    format!(
        "{}:{}:{}",
        display_path(location.uri.as_str(), cwd),
        location.range.start.line + 1,
        location.range.start.character + 1
    )
}

fn location_link_to_location(link: &LocationLink) -> Location {
    Location {
        uri: link.target_uri.clone(),
        range: link.target_selection_range,
    }
}

/// goToDefinition / goToImplementation 共用。
pub fn format_definition(result: &GotoDefinitionResponse, cwd: &std::path::Path) -> String {
    let locations: Vec<Location> = match result {
        GotoDefinitionResponse::Scalar(location) => vec![location.clone()],
        GotoDefinitionResponse::Array(locations) => locations.clone(),
        GotoDefinitionResponse::Link(links) => {
            links.iter().map(location_link_to_location).collect()
        }
    };
    if locations.is_empty() {
        return "No definition found. This may occur if the cursor is not on a symbol, or if \
                the definition is in an external library not indexed by the LSP server."
            .to_string();
    }
    if locations.len() == 1 {
        return format!("Defined in {}", format_location(&locations[0], cwd));
    }
    let list = locations
        .iter()
        .map(|location| format!("  {}", format_location(location, cwd)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Found {} definitions:\n{list}", locations.len())
}

/// findReferences：按文件分组展示（对齐 CC）。
pub fn format_references(locations: &[Location], cwd: &std::path::Path) -> String {
    if locations.is_empty() {
        return "No references found. This may occur if the symbol has no usages, or if the \
                LSP server has not fully indexed the workspace."
            .to_string();
    }
    // 按文件分组，保持出现顺序
    let mut groups: Vec<(String, Vec<&Location>)> = Vec::new();
    for location in locations {
        let key = display_path(location.uri.as_str(), cwd);
        match groups.iter_mut().find(|(path, _)| *path == key) {
            Some((_, bucket)) => bucket.push(location),
            None => groups.push((key, vec![location])),
        }
    }
    let mut out = format!("Found {} reference(s):", locations.len());
    for (path, bucket) in groups {
        out.push_str(&format!("\n{path}:"));
        for location in bucket {
            out.push_str(&format!(
                "\n  Line {}, column {}",
                location.range.start.line + 1,
                location.range.start.character + 1
            ));
        }
    }
    out
}

/// hover：抽取 MarkupContent / MarkedString 内容。
pub fn format_hover(hover: &Hover, cwd: &std::path::Path) -> String {
    let content = hover_contents(hover);
    let Some(content) = content else {
        return "No hover information available. This may occur if the cursor is not on a \
                symbol, or if the LSP server has not fully indexed the file."
            .to_string();
    };
    match &hover.range {
        Some(range) => format!(
            "Hover info at {}:{}:{}:\n\n{}",
            cwd.display(),
            range.start.line + 1,
            range.start.character + 1,
            content
        ),
        None => content,
    }
}

fn hover_contents(hover: &Hover) -> Option<String> {
    match &hover.contents {
        lsp_types::HoverContents::Scalar(marked) => Some(marked_string(marked)),
        lsp_types::HoverContents::Array(marked) => {
            let parts: Vec<String> = marked.iter().map(marked_string).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        lsp_types::HoverContents::Markup(markup) => Some(markup.value.clone()),
    }
}

fn marked_string(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(text) => text.clone(),
        lsp_types::MarkedString::LanguageString(language) => {
            format!("```{}\n{}\n```", language.language, language.value)
        }
    }
}

/// documentSymbol：树形缩进（对齐 CC）。
pub fn format_document_symbols(symbols: &[DocumentSymbol]) -> String {
    if symbols.is_empty() {
        return "No symbols found in document. This may occur if the file is empty, not \
                supported by the LSP server, or if the server has not fully indexed the file."
            .to_string();
    }
    let mut lines = Vec::new();
    collect_document_symbols(symbols, 0, &mut lines);
    lines.join("\n")
}

fn collect_document_symbols(symbols: &[DocumentSymbol], depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    for symbol in symbols {
        let detail = symbol
            .detail
            .as_deref()
            .map(|detail| format!(" — {detail}"))
            .unwrap_or_default();
        lines.push(format!(
            "{indent}{} ({}){}  [L{}:{}]",
            symbol.name,
            symbol_kind(symbol.kind),
            detail,
            symbol.range.start.line + 1,
            symbol.range.start.character + 1
        ));
        if let Some(children) = &symbol.children {
            collect_document_symbols(children, depth + 1, lines);
        }
    }
}

/// workspaceSymbol：单行列表（对齐 CC）。
pub fn format_workspace_symbols(symbols: &[SymbolInformation], cwd: &std::path::Path) -> String {
    if symbols.is_empty() {
        return "No symbols found in workspace. This may occur if the workspace is empty, or \
                if the LSP server has not finished indexing the project."
            .to_string();
    }
    let mut lines = vec![format!("Found {} symbol(s) in workspace:", symbols.len())];
    for symbol in symbols {
        let container = symbol
            .container_name
            .as_deref()
            .map(|name| format!(" in {name}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {} ({}){} - {}",
            symbol.name,
            symbol_kind(symbol.kind),
            container,
            format_location(&symbol.location, cwd)
        ));
    }
    lines.join("\n")
}

/// documentSymbol 的另一种合法形态（LSP 允许 `SymbolInformation[]` 扁平数组）。
/// 统一转成与 DocumentSymbol 树同样的文本行，容器名作为前缀呈现。
pub fn format_symbol_informations(symbols: &[SymbolInformation]) -> String {
    if symbols.is_empty() {
        return "No symbols found in document. This may occur if the file is empty, not \
                supported by the LSP server, or if the server has not fully indexed the file."
            .to_string();
    }
    let mut lines = Vec::new();
    for symbol in symbols {
        let container = symbol
            .container_name
            .as_deref()
            .map(|name| format!("{name}::"))
            .unwrap_or_default();
        lines.push(format!(
            "{}{} ({})  [L{}:{}]",
            container,
            symbol.name,
            symbol_kind(symbol.kind),
            symbol.location.range.start.line + 1,
            symbol.location.range.start.character + 1
        ));
    }
    lines.join("\n")
}

/// SymbolKind → 可读名（未知值透传编号）。
pub fn symbol_kind(kind: lsp_types::SymbolKind) -> &'static str {
    use lsp_types::SymbolKind;
    match kind {
        SymbolKind::FILE => "file",
        SymbolKind::MODULE => "module",
        SymbolKind::NAMESPACE => "namespace",
        SymbolKind::PACKAGE => "package",
        SymbolKind::CLASS => "class",
        SymbolKind::METHOD => "method",
        SymbolKind::PROPERTY => "property",
        SymbolKind::FIELD => "field",
        SymbolKind::CONSTRUCTOR => "constructor",
        SymbolKind::ENUM => "enum",
        SymbolKind::INTERFACE => "interface",
        SymbolKind::FUNCTION => "function",
        SymbolKind::VARIABLE => "variable",
        SymbolKind::CONSTANT => "constant",
        SymbolKind::STRING => "string",
        SymbolKind::NUMBER => "number",
        SymbolKind::BOOLEAN => "boolean",
        SymbolKind::ARRAY => "array",
        SymbolKind::OBJECT => "object",
        SymbolKind::KEY => "key",
        SymbolKind::NULL => "null",
        SymbolKind::ENUM_MEMBER => "enum member",
        SymbolKind::STRUCT => "struct",
        SymbolKind::EVENT => "event",
        SymbolKind::OPERATOR => "operator",
        SymbolKind::TYPE_PARAMETER => "type parameter",
        _ => "symbol",
    }
}

/// callHierarchy item 单行展示。
fn format_call_item(item: &lsp_types::CallHierarchyItem, cwd: &std::path::Path) -> String {
    format!(
        "{} ({}) - {}:{}:{}",
        item.name,
        symbol_kind(item.kind),
        display_path(item.uri.as_str(), cwd),
        item.range.start.line + 1,
        item.range.start.character + 1
    )
}

/// prepareCallHierarchy。
pub fn format_prepare_call_hierarchy(
    items: &[lsp_types::CallHierarchyItem],
    cwd: &std::path::Path,
) -> String {
    if items.is_empty() {
        return "No call hierarchy item found at this position".to_string();
    }
    if items.len() == 1 {
        return format!("Call hierarchy item: {}", format_call_item(&items[0], cwd));
    }
    let list = items
        .iter()
        .map(|item| format!("  {}", format_call_item(item, cwd)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Found {} call hierarchy items:\n{list}", items.len())
}

fn format_call_ranges(ranges: &[lsp_types::Range]) -> String {
    if ranges.is_empty() {
        return String::new();
    }
    let sites = ranges
        .iter()
        .map(|range| format!("{}:{}", range.start.line + 1, range.start.character + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" [calls at: {sites}]")
}

/// incomingCalls：按文件分组（对齐 CC）。
pub fn format_incoming_calls(calls: &[CallHierarchyIncomingCall], cwd: &std::path::Path) -> String {
    if calls.is_empty() {
        return "No incoming calls found (nothing calls this function)".to_string();
    }
    let mut out = format!("Found {} incoming call(s):", calls.len());
    for call in calls {
        out.push_str(&format!("\n{}:", display_path(call.from.uri.as_str(), cwd)));
        out.push_str(&format!(
            "\n  {} ({}) - Line {}{}",
            call.from.name,
            symbol_kind(call.from.kind),
            call.from.range.start.line + 1,
            format_call_ranges(&call.from_ranges)
        ));
    }
    out
}

/// outgoingCalls：按被调文件分组（CC 中 incoming/outgoing 共用结构）。
pub fn format_outgoing_calls(calls: &[CallHierarchyOutgoingCall], cwd: &std::path::Path) -> String {
    if calls.is_empty() {
        return "No outgoing calls found (this function calls nothing)".to_string();
    }
    let mut out = format!("Found {} outgoing call(s):", calls.len());
    for call in calls {
        out.push_str(&format!("\n{}:", display_path(call.to.uri.as_str(), cwd)));
        out.push_str(&format!(
            "\n  {} ({}) - Line {}{}",
            call.to.name,
            symbol_kind(call.to.kind),
            call.to.range.start.line + 1,
            format_call_ranges(&call.from_ranges)
        ));
    }
    out
}

/// LocationLink 也允许（definitions 可能返回 Link 形式）。
pub fn format_definition_from_links(links: &[LocationLink], cwd: &std::path::Path) -> String {
    if links.is_empty() {
        return "No definition found. This may occur if the cursor is not on a symbol, or if \
                the definition is in an external library not indexed by the LSP server."
            .to_string();
    }
    let locations: Vec<Location> = links.iter().map(location_link_to_location).collect();
    let list = locations
        .iter()
        .map(|location| format!("  {}", format_location(location, cwd)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Found {} definitions:\n{list}", locations.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::path::Path;
    use std::str::FromStr;

    fn location(path: &str, line: u32) -> Location {
        Location {
            uri: lsp_types::Uri::from_str(&format!("file://{path}")).unwrap(),
            range: Range {
                start: Position { line, character: 4 },
                end: Position { line, character: 8 },
            },
        }
    }

    #[test]
    fn display_path_prefers_relative() {
        let cwd = Path::new("/proj");
        assert_eq!(display_path("file:///proj/src/main.rs", cwd), "src/main.rs");
        assert_eq!(display_path("file:///other/x.rs", cwd), "/other/x.rs");
        assert_eq!(display_path("not-a-uri", cwd), "not-a-uri");
    }

    #[test]
    fn format_location_is_one_based() {
        let cwd = Path::new("/proj");
        let rendered = format_location(&location("/proj/a.rs", 9), cwd);
        assert_eq!(rendered, "a.rs:10:5");
    }

    #[test]
    fn references_group_by_file() {
        let cwd = Path::new("/proj");
        let locations = vec![
            location("/proj/a.rs", 1),
            location("/proj/b.rs", 5),
            location("/proj/a.rs", 7),
        ];
        let rendered = format_references(&locations, cwd);
        assert!(rendered.starts_with("Found 3 reference(s):"), "{rendered}");
        assert!(rendered.contains("a.rs:"), "{rendered}");
        assert_eq!(rendered.matches("b.rs:").count(), 1, "{rendered}");
    }

    #[test]
    fn empty_results_give_hint_text() {
        let cwd = Path::new("/proj");
        assert!(format_references(&[], cwd).contains("No references found"));
    }
}
