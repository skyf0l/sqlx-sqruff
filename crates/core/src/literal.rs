//! Parse a Rust string-literal token (as produced by `proc_macro2`) into its
//! delimiter shape + inner SQL, and the dedent / re-indent helpers used on the
//! way to and from sqruff. Dependency-free; this is the part proven out in the
//! Python prototype `tmp/sqlx_sqruff.py`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralKind {
    /// `r#"…"#` with `hashes` `#` characters (0 for `r"…"`).
    Raw { hashes: usize },
    /// `"…"` with backslash escapes.
    Normal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLiteral {
    pub kind: LiteralKind,
    /// Inner text. Raw: verbatim. Normal: unescaped.
    pub content: String,
}

impl ParsedLiteral {
    /// Parse the exact source text of a string literal (e.g. `r#"SELECT 1"#`).
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(rest) = text.strip_prefix('r') {
            let hashes = rest.chars().take_while(|&c| c == '#').count();
            let open = rest.get(hashes..)?.strip_prefix('"')?;
            let close: String = std::iter::repeat('#').take(hashes).collect();
            // closing delimiter is `"` + hashes, at the very end of the token
            let inner = open
                .strip_suffix(&format!("\"{close}"))
                .or_else(|| open.strip_suffix('"').filter(|_| hashes == 0))?;
            return Some(Self { kind: LiteralKind::Raw { hashes }, content: inner.to_string() });
        }
        let inner = text.strip_prefix('"')?.strip_suffix('"')?;
        Some(Self { kind: LiteralKind::Normal, content: unescape(inner) })
    }

    pub fn is_raw(&self) -> bool {
        matches!(self.kind, LiteralKind::Raw { .. })
    }

    pub fn is_multiline(&self) -> bool {
        self.content.contains('\n')
    }
}

/// Minimal Rust string unescape (enough for SQL: `\n \t \r \" \\ \0`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Strip leading newlines, trailing whitespace, and the common leading indent,
/// so sqruff sees column-1 SQL. Mirrors Python `textwrap.dedent(s.lstrip("\n").rstrip())`.
pub fn dedent(content: &str) -> String {
    let s = content.trim_start_matches('\n').trim_end();
    if s.is_empty() {
        return String::new();
    }
    let min = s.lines().filter(|l| !l.trim().is_empty()).map(leading_ws_len).min().unwrap_or(0);
    let body = s
        .lines()
        .map(|l| if l.trim().is_empty() { "" } else { l.get(min..).unwrap_or(l) })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{body}\n")
}

/// Common leading indent of the body lines (used to re-indent fixed SQL back).
pub fn block_indent(content: &str) -> String {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.get(..leading_ws_len(l)).unwrap_or("").to_string())
        .min_by_key(|s| s.len())
        .unwrap_or_default()
}

/// The literal's leading and trailing framing, preserved verbatim so a no-op
/// fix rebuilds byte-for-byte. `leading` is the run of `\n` after the opening
/// delimiter; `trailing` is `\n` + closing indent when the closing delimiter
/// sits on its own line, or `""` when it is stuck to the last SQL line.
pub fn framing(content: &str) -> (String, String) {
    let leading: String = content.chars().take_while(|&c| c == '\n').collect();
    let trailing = match content.rsplit_once('\n') {
        Some((_, tail)) if tail.chars().all(|c| c == ' ' || c == '\t') => format!("\n{tail}"),
        _ => String::new(),
    };
    (leading, trailing)
}

/// Prefix every non-empty line of `sql` with `indent`.
pub fn reindent(sql: &str, indent: &str) -> String {
    sql.trim_end_matches('\n')
        .lines()
        .map(|l| if l.trim().is_empty() { String::new() } else { format!("{indent}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

fn leading_ws_len(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_raw_no_hash() {
        let p = ParsedLiteral::parse(r##"r"SELECT 1""##).unwrap();
        assert_eq!(p.kind, LiteralKind::Raw { hashes: 0 });
        assert_eq!(p.content, "SELECT 1");
    }

    #[test]
    fn parse_raw_one_hash_multiline() {
        let text = "r#\"\n    SELECT 1\n    \"#";
        let p = ParsedLiteral::parse(text).unwrap();
        assert_eq!(p.kind, LiteralKind::Raw { hashes: 1 });
        assert!(p.is_multiline());
        assert_eq!(p.content, "\n    SELECT 1\n    ");
    }

    #[test]
    fn parse_normal_with_escapes() {
        let p = ParsedLiteral::parse(r#""a\nb""#).unwrap();
        assert_eq!(p.kind, LiteralKind::Normal);
        assert_eq!(p.content, "a\nb");
    }

    #[test]
    fn dedent_strips_common_indent() {
        let c = "\n                SELECT\n                    a\n                FROM t\n                ";
        assert_eq!(dedent(c), "SELECT\n    a\nFROM t\n");
    }

    #[test]
    fn framing_block_style() {
        let c = "\n            SELECT 1\n            ";
        assert_eq!(block_indent(c), "            ");
        assert_eq!(framing(c), ("\n".to_string(), "\n            ".to_string()));
    }

    #[test]
    fn framing_inline_style_keeps_closing_stuck() {
        // `r#"INSERT …\n   VALUES (…)"#`, no leading/trailing newline.
        let c = "INSERT INTO t\n           VALUES (1)";
        assert_eq!(framing(c), (String::new(), String::new()));
    }

    #[test]
    fn reindent_roundtrips() {
        let sql = "SELECT\n    a\nFROM t\n";
        assert_eq!(reindent(sql, "    "), "    SELECT\n        a\n    FROM t");
    }
}
