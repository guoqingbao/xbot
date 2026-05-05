use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TEXT_STYLE: Style = Style::new().fg(Color::Rgb(200, 200, 210));
const HEADING_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const DIM_STYLE: Style = Style::new().fg(Color::DarkGray);
const ACCENT_STYLE: Style = Style::new().fg(Color::Cyan);
const CODE_BG_STYLE: Style = Style::new()
    .fg(Color::Rgb(223, 207, 175))
    .bg(Color::Rgb(48, 48, 48));
const LINK_STYLE: Style = Style::new()
    .fg(Color::Rgb(135, 175, 255))
    .add_modifier(Modifier::UNDERLINED);
const STRIKE_STYLE: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::CROSSED_OUT);
const KEYWORD_STYLE: Style = Style::new().fg(Color::Rgb(95, 175, 255));
const BUILTIN_STYLE: Style = Style::new().fg(Color::Rgb(175, 135, 255));
const STRING_STYLE: Style = Style::new().fg(Color::Rgb(215, 175, 135));
const NUMBER_STYLE: Style = Style::new().fg(Color::Rgb(215, 135, 95));
const COMMENT_STYLE: Style = Style::new().fg(Color::Rgb(128, 128, 128));
const TABLE_CELL_STYLE: Style = Style::new().fg(Color::Rgb(200, 200, 210));
const TABLE_HEADER_STYLE: Style = Style::new()
    .fg(Color::Rgb(200, 200, 210))
    .add_modifier(Modifier::BOLD);

pub fn markdown_to_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let w = width.max(4);
    let mut out = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();

    let lines_vec: Vec<&str> = text.lines().collect();
    let line_count = lines_vec.len();

    let mut table_ctx = TableContext::new();

    for (line_idx, src) in lines_vec.iter().enumerate() {
        if let Some(lang) = fence_language(src.trim()) {
            if !in_code {
                in_code = true;
                code_lang = lang.to_string();
                let label = if code_lang.is_empty() {
                    "code"
                } else {
                    &code_lang
                };
                out.push(Line::from(Span::styled(format!("  ┌ {label}"), DIM_STYLE)));
            } else {
                let label = if code_lang.is_empty() {
                    "code"
                } else {
                    &code_lang
                };
                out.push(Line::from(Span::styled(format!("  └ {label}"), DIM_STYLE)));
                in_code = false;
                code_lang.clear();
            }
            continue;
        }

        if in_code {
            let mut spans = vec![Span::styled("  │ ", DIM_STYLE)];
            spans.extend(highlight_code_line(src, &code_lang));
            out.push(Line::from(spans));
            continue;
        }

        let trimmed = src.trim();
        if trimmed.is_empty() {
            table_ctx.flush(&mut out, w);
            out.push(Line::from(""));
            continue;
        }

        let is_table_line = trimmed.starts_with('|') && trimmed.ends_with('|');
        let is_sep = is_table_line && is_table_separator(trimmed);

        if is_table_line {
            if is_sep {
                table_ctx.has_header = true;
            } else {
                let cells: Vec<String> = trimmed
                    .trim_matches('|')
                    .split('|')
                    .map(|c| c.trim().to_string())
                    .collect();
                table_ctx.rows.push(cells);
            }

            let next_is_table = line_idx + 1 < line_count && {
                let next = lines_vec[line_idx + 1].trim();
                next.starts_with('|') && next.ends_with('|')
            };
            if !next_is_table {
                table_ctx.flush(&mut out, w);
            }
            continue;
        }

        table_ctx.flush(&mut out, w);

        let indent_len = src.len() - src.trim_start().len();
        let indent_str: String = " ".repeat(indent_len);

        if let Some(content) = parse_heading(trimmed) {
            let mut spans = vec![Span::raw(indent_str)];
            spans.extend(parse_inline(content, HEADING_STYLE));
            wrap_and_push(&mut out, spans, w);
            continue;
        }

        if is_hr(trimmed) {
            out.push(Line::from(Span::styled("─".repeat(w.min(60)), DIM_STYLE)));
            continue;
        }

        if let Some((depth, content)) = parse_blockquote(trimmed) {
            let prefix = "│ ".repeat(depth.max(1));
            let mut spans = vec![Span::styled(prefix, DIM_STYLE)];
            spans.extend(parse_inline(content, DIM_STYLE));
            wrap_and_push(&mut out, spans, w);
            continue;
        }

        if let Some((checked, content)) = parse_task_item(trimmed) {
            let marker = if checked { "[x]" } else { "[ ]" };
            let mut spans = vec![Span::styled(format!("{indent_str}{marker} "), ACCENT_STYLE)];
            spans.extend(parse_inline(content, TEXT_STYLE));
            wrap_and_push(&mut out, spans, w);
            continue;
        }

        if let Some((num, content)) = parse_ordered_item(trimmed) {
            let mut spans = vec![Span::styled(format!("{indent_str}{num}. "), ACCENT_STYLE)];
            spans.extend(parse_inline(content, TEXT_STYLE));
            wrap_and_push(&mut out, spans, w);
            continue;
        }

        if let Some(content) = parse_unordered_item(trimmed) {
            let mut spans = vec![Span::styled(format!("{indent_str}• "), ACCENT_STYLE)];
            spans.extend(parse_inline(content, TEXT_STYLE));
            wrap_and_push(&mut out, spans, w);
            continue;
        }

        let spans = parse_inline(src, TEXT_STYLE);
        wrap_and_push(&mut out, spans, w);
    }

    table_ctx.flush(&mut out, w);

    if in_code {
        let label = if code_lang.is_empty() {
            "code"
        } else {
            &code_lang
        };
        out.push(Line::from(Span::styled(format!("  └ {label}"), DIM_STYLE)));
    }

    out
}

struct TableContext {
    rows: Vec<Vec<String>>,
    has_header: bool,
}

impl TableContext {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            has_header: false,
        }
    }

    fn flush(&mut self, out: &mut Vec<Line<'static>>, max_width: usize) {
        if self.rows.is_empty() {
            return;
        }

        let col_count = self.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if col_count == 0 {
            self.rows.clear();
            self.has_header = false;
            return;
        }

        let mut col_widths = vec![0usize; col_count];
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                col_widths[i] = col_widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }

        let separators_width = (col_count.saturating_sub(1)) * 3;
        let total_content_width: usize = col_widths.iter().sum();
        let total = total_content_width + separators_width;

        if total > max_width && total_content_width > 0 {
            let avail = max_width.saturating_sub(separators_width);
            let scale = avail as f64 / total_content_width as f64;
            for cw in &mut col_widths {
                *cw = ((*cw as f64 * scale).floor() as usize).max(1);
            }
        }

        for (row_idx, row) in self.rows.iter().enumerate() {
            let is_header = self.has_header && row_idx == 0;
            let style = if is_header {
                TABLE_HEADER_STYLE
            } else {
                TABLE_CELL_STYLE
            };

            let mut spans: Vec<Span<'static>> = Vec::new();
            for (i, cell_text) in row.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", DIM_STYLE));
                }
                let cw = col_widths.get(i).copied().unwrap_or(8);
                let cell_w = UnicodeWidthStr::width(cell_text.as_str());
                let padded = if cell_w < cw {
                    format!("{cell_text}{}", " ".repeat(cw - cell_w))
                } else if cell_w > cw && cw > 1 {
                    let mut truncated = String::new();
                    let mut w = 0;
                    for ch in cell_text.chars() {
                        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if w + ch_w >= cw {
                            truncated.push('…');
                            break;
                        }
                        truncated.push(ch);
                        w += ch_w;
                    }
                    truncated
                } else {
                    cell_text.clone()
                };
                spans.push(Span::styled(padded, style));
            }
            out.push(Line::from(spans));

            if is_header {
                let mut sep_spans: Vec<Span<'static>> = Vec::new();
                for (i, &cw) in col_widths.iter().enumerate() {
                    if i > 0 {
                        sep_spans.push(Span::styled("─┼─", DIM_STYLE));
                    }
                    sep_spans.push(Span::styled("─".repeat(cw), DIM_STYLE));
                }
                out.push(Line::from(sep_spans));
            }
        }

        self.rows.clear();
        self.has_header = false;
    }
}

fn wrap_and_push(out: &mut Vec<Line<'static>>, spans: Vec<Span<'static>>, width: usize) {
    let total: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total <= width {
        out.push(Line::from(spans));
        return;
    }

    let mut current: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in spans {
        let style = span.style;
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && cur_w > 0 {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push(Line::from(std::mem::take(&mut current)));
                cur_w = 0;
            }
            buf.push(ch);
            cur_w += cw;
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, style));
        }
    }
    if !current.is_empty() {
        out.push(Line::from(current));
    }
}

fn fence_language(line: &str) -> Option<&str> {
    line.strip_prefix("```").map(str::trim)
}

fn parse_heading(line: &str) -> Option<&str> {
    let level = line.chars().take_while(|c| *c == '#').count();
    (1..=6)
        .contains(&level)
        .then_some(&line[level..])
        .and_then(|r| r.strip_prefix(' '))
}

fn is_hr(line: &str) -> bool {
    let s = line.replace([' ', '\t'], "");
    s.len() >= 3
        && (s.chars().all(|c| c == '-')
            || s.chars().all(|c| c == '*')
            || s.chars().all(|c| c == '_'))
}

fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut rest = line;
    let mut depth = 0usize;
    while let Some(s) = rest.strip_prefix('>') {
        depth += 1;
        rest = s.strip_prefix(' ').unwrap_or(s);
    }
    (depth > 0).then_some((depth, rest.trim_start()))
}

fn parse_task_item(line: &str) -> Option<(bool, &str)> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;
    if let Some(c) = rest.strip_prefix("[ ] ") {
        return Some((false, c));
    }
    if let Some(c) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        return Some((true, c));
    }
    None
}

fn parse_ordered_item(line: &str) -> Option<(usize, &str)> {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || !line[digits..].starts_with(". ") {
        return None;
    }
    let num = line[..digits].parse().ok()?;
    Some((num, &line[digits + 2..]))
}

fn parse_unordered_item(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
}

fn is_table_separator(line: &str) -> bool {
    let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':') && c.contains('-'))
}

pub fn parse_inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    let bytes = text.as_bytes();
    let len = text.len();

    while idx < len {
        if bytes[idx] == b'\\' && idx + 1 < len {
            let ch = text[idx + 1..].chars().next().unwrap();
            out.push(Span::styled(ch.to_string(), base));
            idx += 1 + ch.len_utf8();
            continue;
        }
        if let Some((consumed, rendered)) = try_link(text, idx, base) {
            out.extend(rendered);
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "`") {
            out.push(Span::styled(format!(" {rendered} "), CODE_BG_STYLE));
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "**") {
            out.push(Span::styled(rendered, base.add_modifier(Modifier::BOLD)));
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "__") {
            out.push(Span::styled(rendered, base.add_modifier(Modifier::BOLD)));
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "~~") {
            out.push(Span::styled(rendered, STRIKE_STYLE));
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "*") {
            out.push(Span::styled(rendered, base.add_modifier(Modifier::ITALIC)));
            idx += consumed;
            continue;
        }
        if let Some((consumed, rendered)) = try_delimited(text, idx, "_") {
            out.push(Span::styled(rendered, base.add_modifier(Modifier::ITALIC)));
            idx += consumed;
            continue;
        }

        let ch = text[idx..].chars().next().unwrap();
        let mut plain = String::new();
        plain.push(ch);
        idx += ch.len_utf8();
        while idx < len {
            let c = text[idx..].chars().next().unwrap();
            if matches!(c, '\\' | '`' | '*' | '_' | '~' | '[') {
                break;
            }
            plain.push(c);
            idx += c.len_utf8();
        }
        out.push(Span::styled(plain, base));
    }

    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
    }
    out
}

fn try_link(text: &str, start: usize, _base: Style) -> Option<(usize, Vec<Span<'static>>)> {
    let rest = &text[start..];
    if !rest.starts_with('[') {
        return None;
    }
    let close_bracket = rest.find("](")?;
    let label = &rest[1..close_bracket];
    let url_start = close_bracket + 2;
    let close_paren = rest[url_start..].find(')')?;
    let url = &rest[url_start..url_start + close_paren];
    let consumed = url_start + close_paren + 1;
    Some((
        consumed,
        vec![
            Span::styled(label.to_string(), LINK_STYLE),
            Span::styled(format!(" ({url})"), DIM_STYLE),
        ],
    ))
}

fn try_delimited(text: &str, start: usize, delim: &str) -> Option<(usize, String)> {
    let rest = &text[start..];
    let inner = rest.strip_prefix(delim)?;
    let end = find_closing(inner, delim)?;
    let content = &inner[..end];
    if content.is_empty() {
        return None;
    }
    Some((delim.len() * 2 + end, content.to_string()))
}

fn find_closing(text: &str, delim: &str) -> Option<usize> {
    let mut esc = false;
    let mut idx = 0usize;
    while idx < text.len() {
        let ch = text[idx..].chars().next()?;
        if esc {
            esc = false;
            idx += ch.len_utf8();
            continue;
        }
        if ch == '\\' {
            esc = true;
            idx += 1;
            continue;
        }
        if text[idx..].starts_with(delim) {
            return Some(idx);
        }
        idx += ch.len_utf8();
    }
    None
}

fn highlight_code_line(line: &str, language: &str) -> Vec<Span<'static>> {
    if line.is_empty() {
        return vec![Span::raw(String::new())];
    }
    let lang = normalize_lang(language);
    let (code, comment) = split_comment(line, lang);
    let mut spans = highlight_tokens(code, lang);
    if let Some(c) = comment {
        spans.push(Span::styled(c.to_string(), COMMENT_STYLE));
    }
    spans
}

fn normalize_lang(l: &str) -> &'static str {
    match l.to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rust",
        "py" | "python" => "python",
        "js" | "javascript" => "javascript",
        "ts" | "typescript" => "typescript",
        "jsx" => "jsx",
        "tsx" => "tsx",
        "sh" | "zsh" | "bash" | "shell" => "bash",
        "yml" | "yaml" => "yaml",
        "go" | "golang" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "c++" | "cxx" => "cpp",
        "rb" | "ruby" => "ruby",
        "sql" => "sql",
        "json" => "json",
        "toml" => "toml",
        "dockerfile" => "dockerfile",
        "swift" => "swift",
        "kotlin" | "kt" => "kotlin",
        "csharp" | "cs" | "c#" => "csharp",
        _ => "text",
    }
}

fn split_comment<'a>(line: &'a str, lang: &str) -> (&'a str, Option<&'a str>) {
    let delim = match lang {
        "python" | "bash" | "yaml" | "ruby" | "dockerfile" => Some("#"),
        "sql" => Some("--"),
        "rust" | "javascript" | "typescript" | "jsx" | "tsx" | "java" | "go" | "c" | "cpp"
        | "swift" | "kotlin" | "csharp" => Some("//"),
        _ => None,
    };
    let Some(d) = delim else {
        return (line, None);
    };
    let mut quote: Option<char> = None;
    let mut esc = false;
    for (i, ch) in line.char_indices() {
        if let Some(q) = quote {
            if esc {
                esc = false;
                continue;
            }
            if ch == '\\' {
                esc = true;
                continue;
            }
            if ch == q {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            quote = Some(ch);
            continue;
        }
        if line[i..].starts_with(d) {
            return (&line[..i], Some(&line[i..]));
        }
    }
    (line, None)
}

fn highlight_tokens(code: &str, lang: &str) -> Vec<Span<'static>> {
    let kw = lang_keywords(lang);
    let bi = builtin_keywords(lang);
    let chars: Vec<char> = code.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if matches!(ch, '"' | '\'' | '`') {
            let start = i;
            i += 1;
            let mut esc = false;
            while i < chars.len() {
                if esc {
                    esc = false;
                } else if chars[i] == '\\' {
                    esc = true;
                } else if chars[i] == ch {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            out.push(Span::styled(s, STRING_STYLE));
            continue;
        }
        if ch.is_ascii_digit()
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric() && chars[i - 1] != '_')
        {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_hexdigit()
                    || matches!(chars[i], '_' | '.' | 'x' | 'X' | 'o' | 'O' | 'b' | 'B'))
            {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            out.push(Span::styled(s, NUMBER_STYLE));
            continue;
        }
        if ch == '_' || ch.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            if kw.contains(&token.as_str()) {
                out.push(Span::styled(token, KEYWORD_STYLE));
            } else if bi.contains(&token.as_str()) {
                out.push(Span::styled(token, BUILTIN_STYLE));
            } else {
                out.push(Span::raw(token));
            }
            continue;
        }
        out.push(Span::raw(ch.to_string()));
        i += 1;
    }
    if out.is_empty() {
        out.push(Span::raw(String::new()));
    }
    out
}

fn lang_keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &[
            "fn", "let", "mut", "pub", "impl", "struct", "enum", "trait", "async", "await",
            "match", "if", "else", "for", "while", "loop", "return", "use", "mod", "const",
            "static", "where", "Self", "self", "type", "crate", "super", "dyn", "move", "ref",
            "unsafe", "extern",
        ],
        "python" => &[
            "def", "class", "async", "await", "if", "elif", "else", "for", "while", "return",
            "import", "from", "try", "except", "finally", "with", "as", "pass", "yield", "raise",
            "lambda", "and", "or", "not", "in", "is",
        ],
        "javascript" | "typescript" | "jsx" | "tsx" => &[
            "function",
            "const",
            "let",
            "var",
            "class",
            "extends",
            "async",
            "await",
            "if",
            "else",
            "for",
            "while",
            "return",
            "import",
            "from",
            "export",
            "new",
            "switch",
            "case",
            "default",
            "try",
            "catch",
            "throw",
            "interface",
            "type",
            "enum",
        ],
        "bash" => &[
            "if", "then", "else", "elif", "fi", "for", "do", "done", "case", "esac", "function",
            "in", "while", "until", "select", "local", "export",
        ],
        "go" => &[
            "func",
            "var",
            "const",
            "type",
            "struct",
            "interface",
            "map",
            "chan",
            "if",
            "else",
            "for",
            "range",
            "switch",
            "case",
            "default",
            "return",
            "go",
            "defer",
            "select",
            "package",
            "import",
        ],
        _ => &[
            "if", "else", "for", "while", "return", "class", "function", "const", "let", "var",
        ],
    }
}

fn builtin_keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &[
            "Some", "None", "Ok", "Err", "true", "false", "Vec", "String", "Box", "Arc", "Result",
            "Option",
        ],
        "python" => &[
            "True", "False", "None", "print", "len", "range", "str", "int", "float", "list",
            "dict", "set",
        ],
        "javascript" | "typescript" | "jsx" | "tsx" => &[
            "true",
            "false",
            "null",
            "undefined",
            "console",
            "Promise",
            "Array",
            "Object",
        ],
        "go" => &[
            "true", "false", "nil", "make", "len", "cap", "append", "copy", "close", "delete",
            "new",
        ],
        _ => &["true", "false", "null"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_parsed() {
        let lines = markdown_to_lines("# Hello World", 80);
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Hello World"));
    }

    #[test]
    fn code_block_fenced() {
        let md = "```rust\nlet x = 42;\n```";
        let lines = markdown_to_lines(md, 80);
        assert!(lines.len() >= 3);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains("rust"));
    }

    #[test]
    fn inline_bold() {
        let spans = parse_inline("hello **world**", Style::default());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn unordered_list() {
        let lines = markdown_to_lines("- item one\n- item two", 80);
        assert_eq!(lines.len(), 2);
        let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.contains('•'));
    }

    #[test]
    fn table_rendering() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |";
        let lines = markdown_to_lines(md, 80);
        assert!(lines.len() >= 3, "got {} lines", lines.len());
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("Name"), "header: {header}");
        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains('─'), "sep: {sep}");
    }

    #[test]
    fn table_no_header() {
        let md = "| A | B |\n| 1 | 2 |";
        let lines = markdown_to_lines(md, 80);
        assert_eq!(lines.len(), 2);
    }
}
