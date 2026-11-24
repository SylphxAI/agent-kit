//! Outbound native Rich Markdown and HTML formatting.

/// Tracks fenced code blocks (` ``` ` or `~~~`, three or more) line by line.
/// A fence closes only on a line of the same character, at least as long as the
/// opener, with nothing after it but whitespace.
#[derive(Default)]
struct FenceTracker {
    open: Option<(u8, usize)>,
}

impl FenceTracker {
    /// Feeds one line and reports whether it is a fence line or fenced content.
    fn advance(&mut self, line: &str) -> bool {
        let marker = fence_marker(line);
        match (self.open, marker) {
            (Some((ch, len)), Some((m_ch, m_len, rest)))
                if m_ch == ch && m_len >= len && rest.trim().is_empty() =>
            {
                self.open = None;
                true
            }
            (Some(_), _) => true,
            (None, Some((m_ch, m_len, rest))) => {
                // A backtick fence's info string may not contain a backtick.
                if m_ch == b'`' && rest.contains('`') {
                    return false;
                }
                self.open = Some((m_ch, m_len));
                true
            }
            (None, None) => false,
        }
    }
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let mut trimmed = line;
    loop {
        let content = trimmed.trim_start_matches(' ');
        if trimmed.len() - content.len() > 3 {
            return None;
        }
        trimmed = content;
        if let Some(quote) = trimmed.strip_prefix('>') {
            trimmed = quote.strip_prefix(' ').unwrap_or(quote);
        } else {
            break;
        }
    }
    let ch = *trimmed.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|b| *b == ch).count();
    (len >= 3).then(|| (ch, len, &trimmed[len..]))
}

/// Whether a line holds no content, counting blockquote markers as empty.
fn is_blank_markdown_line(line: &str) -> bool {
    line.trim_matches(|c: char| c == '>' || c.is_whitespace())
        .is_empty()
}

/// Byte ranges of matched inline code spans. Unmatched backticks are prose.
/// Paragraph and fenced-block boundaries prevent an opener from consuming a
/// later, unrelated block. Backslashes only escape ticks outside code spans.
fn inline_code_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    fn scan(text: &str, base: usize, spans: &mut Vec<std::ops::Range<usize>>) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            let width = i - start;
            let mut j = i;
            while j < bytes.len() {
                if bytes[j] != b'`' {
                    j += 1;
                    continue;
                }
                let close = j;
                while j < bytes.len() && bytes[j] == b'`' {
                    j += 1;
                }
                if j - close == width {
                    spans.push(base + start..base + j);
                    i = j;
                    break;
                }
            }
        }
    }
    let mut spans = Vec::new();
    let mut fence = FenceTracker::default();
    let mut start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if fence.advance(line.trim_end_matches(['\r', '\n'])) || is_blank_markdown_line(line) {
            scan(&text[start..offset], start, &mut spans);
            start = offset + line.len();
        }
        offset += line.len();
    }
    scan(&text[start..], start, &mut spans);
    spans
}

/// Turns each single `\n` between two content lines into a CommonMark hard
/// break (`  \n`), so `sendRichMessage` keeps the line breaks the author wrote
/// instead of folding them into one paragraph. CRLF is normalized to LF. Blank
/// lines stay paragraph breaks. Fenced code blocks, table rows, the line before
/// a table or fence, headings, lines already ending in a hard break, and
/// indented code blocks are left as written.
pub fn preserve_rich_markdown_line_breaks(text: &str) -> String {
    if !text.contains('\n') {
        return text.to_string();
    }
    let normalized = text.replace("\r\n", "\n");
    let spans = inline_code_spans(&normalized);
    let mut offset = 0;
    let lines: Vec<&str> = normalized
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut out = String::with_capacity(text.len() + lines.len() * 2);
    let mut fence = FenceTracker::default();
    let mut in_indented_code = false;
    let mut prev_blank = true;
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(line);
        let newline = offset + line.len();
        offset = newline + 1;
        if fence.advance(line) {
            prev_blank = false;
            in_indented_code = false;
            continue;
        }
        let blank = is_blank_markdown_line(line);
        let indent = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum::<usize>();
        if !blank {
            in_indented_code = indent >= 4 && (prev_blank || in_indented_code);
        }
        prev_blank = blank;
        let Some(next) = lines.get(idx + 1) else {
            continue;
        };
        let trimmed = line.trim();
        let keep = blank
            || in_indented_code
            || is_blank_markdown_line(next)
            || line.ends_with("  ")
            || line.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
            || spans.iter().any(|span| span.contains(&newline))
            || is_markdown_table_row(trimmed)
            || is_markdown_table_row(next.trim())
            || fence_marker(next).is_some()
            || strip_markdown_heading(trimmed).is_some();
        if !keep {
            out.push_str("  ");
        }
    }
    out
}

/// Prepares agent text for `rich_message.markdown`: keeps single line breaks,
/// then escapes currency dollar signs.
pub fn rich_markdown(text: &str) -> String {
    escape_rich_markdown_dollars(&preserve_rich_markdown_line_breaks(text))
}

/// Escapes unescaped `$` characters outside inline code spans (`` `...` ``) and
/// fenced code blocks (` ``` ` or `~~~`) so Telegram Bot API 10.1+ `sendRichMessage`
/// (`rich_message.markdown`) never misinterprets currency pairs (e.g. `S$123.45 同 US$170`
/// or `HK$100 and HK$200`) as a `RichTextMathematicalExpression`.
pub fn escape_rich_markdown_dollars(text: &str) -> String {
    if !text.contains('$') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut fence = FenceTracker::default();
    let spans = inline_code_spans(text);
    let mut offset = 0;
    for (line_idx, line) in text.split('\n').enumerate() {
        if line_idx > 0 {
            out.push('\n');
        }
        let base = offset;
        offset += line.len() + 1;
        if fence.advance(line) {
            out.push_str(line);
            continue;
        }
        let mut chars = line.char_indices().peekable();
        while let Some((idx, c)) = chars.next() {
            if spans.iter().any(|span| span.contains(&(base + idx))) {
                out.push(c);
                continue;
            }
            if c == '\\' {
                out.push(c);
                if let Some((_, escaped)) = chars.next() {
                    out.push(escaped);
                }
                continue;
            }
            if c == '$' {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// Format an interactive checklist in Markdown so it renders cleanly in any Telegram chat
/// and can be updated in-place via `interact(action="check" | "uncheck")`.
pub fn format_checklist_markdown(title: &str, tasks: &[String]) -> String {
    let mut lines = Vec::with_capacity(tasks.len() + 1);
    lines.push(format!("☑️ **{}**", title.trim()));
    for (idx, raw) in tasks.iter().enumerate() {
        let trimmed = raw.trim();
        let (done, label) = if let Some(rest) = trimmed
            .strip_prefix("[x]")
            .or_else(|| trimmed.strip_prefix("[X]"))
            .or_else(|| trimmed.strip_prefix("☑"))
            .or_else(|| trimmed.strip_prefix("✅"))
        {
            (true, rest.trim())
        } else if let Some(rest) = trimmed
            .strip_prefix("[ ]")
            .or_else(|| trimmed.strip_prefix("☐"))
        {
            (false, rest.trim())
        } else {
            (false, trimmed)
        };
        let box_char = if done { "☑" } else { "☐" };
        lines.push(format!("{box_char} {}. {label}", idx + 1));
    }
    lines.join("\n")
}

/// Convert CommonMark/GFM Markdown into Telegram-compatible rich HTML (`parse_mode: "HTML"`).
///
/// Escapes raw `&`, `<`, `>` outside tags and converts fenced code blocks,
/// inline code spans, bold (`**...**`), italic (`*...*` / word-boundary `_..._`),
/// strikethrough (`~~...~~`), spoilers (`||...||`), headings, blockquotes, and
/// links (`[label](url)`) into balanced Telegram HTML tags.
pub fn markdown_to_telegram_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            let lang: String = fence_rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '+')
                .collect();
            let mut code_lines = Vec::new();
            let mut closed = false;
            let mut j = i + 1;
            while j < lines.len() {
                if lines[j].trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                code_lines.push(lines[j]);
                j += 1;
            }
            if closed {
                if i > 0 {
                    out.push('\n');
                }
                let escaped = escape_html(&code_lines.join("\n"));
                if lang.is_empty() {
                    out.push_str("<pre>");
                    out.push_str(&escaped);
                    out.push_str("</pre>");
                } else {
                    out.push_str("<pre><code class=\"language-");
                    out.push_str(&lang);
                    out.push_str("\">");
                    out.push_str(&escaped);
                    out.push_str("</code></pre>");
                }
                i = j + 1;
                continue;
            }
        }
        if trimmed.starts_with("> ") || trimmed == ">" {
            let start_i = i;
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some(rest) = t.strip_prefix("> ") {
                    quote_lines.push(format_inline_markdown(rest));
                    i += 1;
                } else if t == ">" {
                    quote_lines.push(String::new());
                    i += 1;
                } else {
                    break;
                }
            }
            if start_i > 0 {
                out.push('\n');
            }
            out.push_str("<blockquote>");
            out.push_str(&quote_lines.join("\n"));
            out.push_str("</blockquote>");
            continue;
        }
        if i + 1 < lines.len()
            && is_markdown_table_row(trimmed)
            && is_markdown_table_separator(lines[i + 1].trim())
        {
            let start_i = i;
            let headers = parse_markdown_table_cells(trimmed);
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() {
                let row_trimmed = lines[i].trim();
                if !is_markdown_table_row(row_trimmed) {
                    break;
                }
                if !is_markdown_table_separator(row_trimmed) {
                    rows.push(parse_markdown_table_cells(row_trimmed));
                }
                i += 1;
            }
            if start_i > 0 {
                out.push('\n');
            }
            out.push_str(&format_markdown_table_html(&headers, &rows));
            continue;
        }
        if i > 0 {
            out.push('\n');
        }
        if let Some(heading) = strip_markdown_heading(trimmed) {
            out.push_str("<b>");
            out.push_str(&format_inline_markdown(heading));
            out.push_str("</b>");
        } else {
            out.push_str(&format_inline_markdown(line));
        }
        i += 1;
    }
    out
}

fn is_markdown_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() >= 3
}

fn is_markdown_table_separator(line: &str) -> bool {
    if !is_markdown_table_row(line) {
        return false;
    }
    let cells = parse_markdown_table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let s = c.trim();
            !s.is_empty()
                && s.chars().any(|ch| ch == '-')
                && s.chars()
                    .all(|ch| ch == '-' || ch == ':' || ch.is_ascii_whitespace())
        })
}

fn parse_markdown_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t
        .strip_prefix('|')
        .unwrap_or(t)
        .strip_suffix('|')
        .unwrap_or(t);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut in_ticks = 0usize;
    let chars: Vec<char> = inner.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let ch = chars[idx];
        if ch == '\\' && idx + 1 < chars.len() && chars[idx + 1] == '|' {
            current.push('|');
            idx += 2;
            continue;
        }
        if ch == '`' {
            let mut run = 1;
            while idx + run < chars.len() && chars[idx + run] == '`' {
                run += 1;
            }
            for _ in 0..run {
                current.push('`');
            }
            if in_ticks == 0 {
                in_ticks = run;
            } else if in_ticks == run {
                in_ticks = 0;
            }
            idx += run;
            continue;
        }
        if ch == '|' && in_ticks == 0 {
            cells.push(current.trim().to_string());
            current.clear();
            idx += 1;
            continue;
        }
        current.push(ch);
        idx += 1;
    }
    cells.push(current.trim().to_string());
    cells
}

fn format_table_cell_inline(cell: &str) -> String {
    let normalized = replace_html_br(cell, " / ");
    format_inline_markdown(normalized.trim())
}

fn replace_html_br(s: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(consumed) = match_html_br(&s[i..]) {
            out.push_str(replacement);
            i += consumed;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn match_html_br(s: &str) -> Option<usize> {
    if !s.starts_with('<') {
        return None;
    }
    let close = s.find('>')?;
    let inside = s[1..close].trim();
    let inside = inside.strip_suffix('/').unwrap_or(inside).trim();
    if inside.eq_ignore_ascii_case("br") {
        Some(close + 1)
    } else {
        None
    }
}

fn format_markdown_table_html(headers: &[String], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        let joined: Vec<String> = headers
            .iter()
            .map(|h| format!("<b>{}</b>", format_table_cell_inline(h)))
            .collect();
        return format!("<blockquote>{}</blockquote>", joined.join(" · "));
    }
    let first_hdr = headers.first().map(|s| s.trim()).unwrap_or("");
    let first_is_index = matches!(
        first_hdr.to_ascii_lowercase().as_str(),
        "#" | "no" | "no." | "id" | "序號" | "編號"
    );
    if headers.len() <= 2 {
        let mut lines = Vec::with_capacity(rows.len());
        for row in rows {
            let c0 = row
                .first()
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            let c1 = row
                .get(1)
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            if c1.is_empty() {
                lines.push(format!("• {c0}"));
            } else {
                lines.push(format!("• {c0} — {c1}"));
            }
        }
        return format!("<blockquote>{}</blockquote>", lines.join("\n"));
    }
    let mut cards = Vec::with_capacity(rows.len());
    for row in rows {
        let mut card_lines = Vec::new();
        let start_col = if first_is_index && headers.len() >= 2 {
            let c0 = row
                .first()
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            let c1 = row
                .get(1)
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            card_lines.push(format!("{c0} · {c1}"));
            2
        } else {
            let c0 = row
                .first()
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            let h0 = headers
                .first()
                .map(|s| format_table_cell_inline(s))
                .unwrap_or_default();
            if c0.starts_with("<b>") || h0.is_empty() {
                card_lines.push(c0);
            } else {
                card_lines.push(format!("<b>{h0}</b>: {c0}"));
            }
            1
        };
        for col in start_col..headers.len().max(row.len()) {
            let val = row.get(col).map(|s| s.trim()).unwrap_or("");
            if val.is_empty() || val == "-" {
                continue;
            }
            let formatted_val = format_table_cell_inline(val);
            let hdr = headers.get(col).map(|s| s.trim()).unwrap_or("");
            if hdr.is_empty() {
                card_lines.push(format!("• {formatted_val}"));
            } else {
                let formatted_hdr = format_table_cell_inline(hdr);
                card_lines.push(format!("• <b>{formatted_hdr}</b>: {formatted_val}"));
            }
        }
        cards.push(format!(
            "<blockquote>{}</blockquote>",
            card_lines.join("\n")
        ));
    }
    cards.join("\n")
}

fn strip_markdown_heading(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        if let Some(stripped) = rest.strip_prefix(' ') {
            let trimmed = stripped.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn format_inline_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Inline code span: `...`
        if bytes[i] == b'`' {
            let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let start = i + ticks;
            if let Some(rel_end) = find_closing_backticks(&input[start..], ticks) {
                let code = &input[start..start + rel_end];
                out.push_str("<code>");
                out.push_str(&escape_html(code));
                out.push_str("</code>");
                i = start + rel_end + ticks;
                continue;
            }
        }
        // Markdown link: [label](url)
        if bytes[i] == b'['
            && let Some((label, url, consumed)) = parse_markdown_link(&input[i..])
        {
            out.push_str("<a href=\"");
            out.push_str(&escape_html_attr(url));
            out.push_str("\">");
            out.push_str(&format_inline_markdown(label));
            out.push_str("</a>");
            i += consumed;
            continue;
        }
        // Bold + Italic: ***...***
        if input[i..].starts_with("***")
            && let Some(end) = find_closing_delim(&input[i + 3..], "***")
            && end > 0
        {
            out.push_str("<b><i>");
            out.push_str(&format_inline_markdown(&input[i + 3..i + 3 + end]));
            out.push_str("</i></b>");
            i += 3 + end + 3;
            continue;
        }
        // Bold: **...**
        if input[i..].starts_with("**")
            && let Some(end) = find_closing_delim(&input[i + 2..], "**")
            && end > 0
        {
            out.push_str("<b>");
            out.push_str(&format_inline_markdown(&input[i + 2..i + 2 + end]));
            out.push_str("</b>");
            i += 2 + end + 2;
            continue;
        }
        // Strikethrough: ~~...~~
        if input[i..].starts_with("~~")
            && let Some(end) = find_closing_delim(&input[i + 2..], "~~")
            && end > 0
        {
            out.push_str("<s>");
            out.push_str(&format_inline_markdown(&input[i + 2..i + 2 + end]));
            out.push_str("</s>");
            i += 2 + end + 2;
            continue;
        }
        // Spoiler: ||...||
        if input[i..].starts_with("||")
            && let Some(end) = find_closing_delim(&input[i + 2..], "||")
            && end > 0
        {
            out.push_str("<tg-spoiler>");
            out.push_str(&format_inline_markdown(&input[i + 2..i + 2 + end]));
            out.push_str("</tg-spoiler>");
            i += 2 + end + 2;
            continue;
        }
        // Italic: *...* (not followed/preceded by space)
        if bytes[i] == b'*'
            && i + 1 < len
            && !bytes[i + 1].is_ascii_whitespace()
            && bytes[i + 1] != b'*'
            && let Some(end) = find_closing_single_star(&input[i + 1..])
        {
            out.push_str("<i>");
            out.push_str(&format_inline_markdown(&input[i + 1..i + 1 + end]));
            out.push_str("</i>");
            i += 1 + end + 1;
            continue;
        }
        // HTML <br> / <br/> / <br /> line break
        if bytes[i] == b'<'
            && let Some(consumed) = match_html_br(&input[i..])
        {
            out.push('\n');
            i += consumed;
            continue;
        }
        let ch = input[i..].chars().next().unwrap();
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
        i += ch.len_utf8();
    }
    out
}

fn find_closing_backticks(s: &str, ticks: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if run == ticks {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_closing_delim(s: &str, delim: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if let Some(end) = find_closing_backticks(&s[i + ticks..], ticks) {
                i += ticks + end + ticks;
                continue;
            }
        }
        if s[i..].starts_with(delim) {
            return Some(i);
        }
        let ch = s[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

fn find_closing_single_star(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if let Some(end) = find_closing_backticks(&s[i + ticks..], ticks) {
                i += ticks + end + ticks;
                continue;
            }
        }
        if bytes[i] == b'*' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                continue;
            }
            if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
                return Some(i);
            }
        }
        let ch = s[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

fn parse_markdown_link(s: &str) -> Option<(&str, &str, usize)> {
    let close_bracket = s.find("](")?;
    if close_bracket <= 1 || s[1..close_bracket].contains('[') {
        return None;
    }
    let label = &s[1..close_bracket];
    let after = &s[close_bracket + 2..];
    let close_paren = after.find(')')?;
    let url = after[..close_paren].trim();
    if !(url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("tg://")
        || url.starts_with("mailto:"))
    {
        return None;
    }
    if url.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some((label, url, close_bracket + 2 + close_paren + 1))
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}
