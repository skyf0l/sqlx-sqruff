//! Parse a Rust string-literal token (as produced by `proc_macro2`) into its
//! delimiter shape + inner SQL, and the dedent / re-indent helpers used on the
//! way to and from sqruff.

use std::{iter::Peekable, ops::Range, str::CharIndices};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralKind {
    /// `r#"..."#` with `hashes` `#` characters (0 for `r"..."`).
    Raw { hashes: usize },
    /// `"..."` with backslash escapes.
    Normal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLiteral {
    pub kind: LiteralKind,
    /// The literal's value as the compiler sees it: escapes resolved and CRLF
    /// line endings turned into LF.
    pub content: String,
    /// Whether the literal's source writes its line endings as CRLF.
    pub crlf: bool,
    /// Whether `content` can be written back in this literal's delimiter without
    /// changing its value: always for raw strings, and for normal strings only
    /// when every escape is `\"` or `\\`.
    pub rewritable: bool,
    /// Byte offset, in the literal's source text, of the source character that
    /// each `content` char came from.
    offsets: Vec<usize>,
    /// Byte offset of the closing delimiter in the literal's source text.
    close: usize,
}

impl ParsedLiteral {
    /// Parse the exact source text of a string literal (e.g. `r#"SELECT 1"#`).
    pub fn parse(text: &str) -> Option<Self> {
        let crlf = text.contains("\r\n");
        let mut content = String::new();
        let mut offsets = Vec::new();
        if let Some(rest) = text.strip_prefix('r') {
            let hashes = rest.chars().take_while(|&c| c == '#').count();
            let open = rest.get(hashes..)?.strip_prefix('"')?;
            let close = "#".repeat(hashes);
            // closing delimiter is `"` + hashes, at the very end of the token
            let inner = open
                .strip_suffix(&format!("\"{close}"))
                .or_else(|| open.strip_suffix('"').filter(|_| hashes == 0))?;
            let start = hashes + 2;
            let mut chars = inner.char_indices().peekable();
            while let Some((i, c)) = chars.next() {
                let is_crlf = c == '\r' && chars.next_if(|&(_, n)| n == '\n').is_some();
                content.push(if is_crlf { '\n' } else { c });
                offsets.push(start + i);
            }
            return Some(Self {
                kind: LiteralKind::Raw { hashes },
                content,
                crlf,
                rewritable: true,
                offsets,
                close: start + inner.len(),
            });
        }
        let inner = text.strip_prefix('"')?.strip_suffix('"')?;
        let mut rewritable = true;
        let mut chars = inner.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let value = match c {
                '\\' => {
                    let (_, escape) = chars.next()?;
                    if !matches!(escape, '"' | '\\') {
                        rewritable = false;
                    }
                    match escape {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '0' => '\0',
                        '\'' => '\'',
                        '"' => '"',
                        'x' => {
                            let high = chars.next()?.1.to_digit(16)?;
                            let low = chars.next()?.1.to_digit(16)?;
                            char::from_u32(high * 16 + low).filter(char::is_ascii)?
                        }
                        'u' => unicode_escape(&mut chars)?,
                        '\n' | '\r' => {
                            while chars.next_if(is_continuation_space).is_some() {}
                            continue;
                        }
                        _ => return None,
                    }
                }
                '\r' => {
                    chars.next_if(|&(_, n)| n == '\n')?;
                    '\n'
                }
                c => c,
            };
            content.push(value);
            offsets.push(1 + i);
        }
        Some(Self {
            kind: LiteralKind::Normal,
            content,
            crlf,
            rewritable,
            offsets,
            close: 1 + inner.len(),
        })
    }

    pub fn is_multiline(&self) -> bool {
        self.content.contains('\n')
    }

    /// Byte offset, in the literal's source text, of `content` char `index`. An
    /// index past the end of `content` gives the closing delimiter.
    pub fn source_offset(&self, index: usize) -> usize {
        self.offsets.get(index).copied().unwrap_or(self.close)
    }
}

/// Whitespace that a `\` line continuation skips.
fn is_continuation_space(&(_, c): &(usize, char)) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// The rest of a `\u{...}` escape, after the `u`.
fn unicode_escape(chars: &mut Peekable<CharIndices>) -> Option<char> {
    chars.next_if(|&(_, c)| c == '{')?;
    let mut value = 0u32;
    let mut digits = 0;
    loop {
        match chars.next()?.1 {
            '}' if digits > 0 => return char::from_u32(value),
            '_' if digits > 0 => {}
            c => {
                if digits == 6 {
                    return None;
                }
                value = value * 16 + c.to_digit(16)?;
                digits += 1;
            }
        }
    }
}

/// The SQL that sqruff lints and fixes for one literal, with the way back from
/// a position in that SQL to a char of the literal's content.
pub struct LintView {
    pub sql: String,
    /// The whitespace prefix removed from the start of each line of `sql`
    /// (except whitespace-only lines and a first line stuck to the opening
    /// delimiter). [`reindent`] and [`reindent_keep_first`] add exactly this
    /// string back.
    pub indent: String,
    /// Content char index that each `sql` char came from.
    origin: Vec<usize>,
    /// Index in `sql` (in chars) where each line starts.
    line_starts: Vec<usize>,
    /// Content length in chars.
    end: usize,
}

impl LintView {
    /// Strip leading newlines, trailing whitespace and the leading whitespace
    /// prefix common to every non-blank line, so sqruff sees column-1 SQL. A
    /// whitespace-only line is kept exactly as written: it can be part of a
    /// string constant, and once the prefix was removed from it, re-indenting
    /// could not tell an empty line from one that held only the prefix. When
    /// the first SQL line is stuck to the opening delimiter and more lines
    /// follow, that first line has no indent of its own, so it is kept as is
    /// and the prefix is taken from the continuation lines only. This is the
    /// same split `fix` uses to re-indent.
    pub fn new(content: &str) -> Self {
        let chars: Vec<char> = content.chars().collect();
        let start = chars.iter().take_while(|&&c| c == '\n').count();
        let stop = chars.iter().rposition(|c| !c.is_whitespace()).map_or(start, |i| i + 1);
        let mut view = Self {
            sql: String::new(),
            indent: String::new(),
            origin: Vec::new(),
            line_starts: vec![0],
            end: chars.len(),
        };
        if stop <= start {
            return view;
        }
        let stuck = start == 0 && content.contains('\n');
        let mut lines = Vec::new();
        let mut line_start = start;
        for (i, &c) in chars.iter().enumerate().take(stop).skip(start) {
            if c == '\n' {
                lines.push(line_start..i);
                line_start = i + 1;
            }
        }
        lines.push(line_start..stop);
        let is_blank = |l: &Range<usize>| chars[l.clone()].iter().all(|c| c.is_whitespace());
        let leading_whitespace = |l: &Range<usize>| {
            let text = &chars[l.clone()];
            &text[..text.iter().take_while(|c| c.is_whitespace()).count()]
        };
        let indent: &[char] = lines
            .iter()
            .skip(usize::from(stuck))
            .filter(|l| !is_blank(l))
            .map(leading_whitespace)
            .reduce(|common, next| {
                &common[..common.iter().zip(next).take_while(|(a, b)| a == b).count()]
            })
            .unwrap_or_default();
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                view.push('\n', line.start - 1);
                view.line_starts.push(view.origin.len());
            }
            let keeps_indent = (stuck && n == 0) || is_blank(line);
            let skip = if keeps_indent { 0 } else { indent.len() };
            for i in line.clone().skip(skip) {
                view.push(chars[i], i);
            }
        }
        view.push('\n', stop);
        view.indent = indent.iter().collect();
        view
    }

    fn push(&mut self, c: char, origin: usize) {
        self.sql.push(c);
        self.origin.push(origin);
    }

    /// Content char index of a sqruff position (1-based line, 1-based column in
    /// chars). Line 0 is read as line 1. A column past the end of its line is
    /// clamped to the line's end; a position past the end of the SQL gives the
    /// end of the content.
    pub fn content_index(&self, line: usize, col: usize) -> usize {
        let line = line.max(1);
        let Some(&line_start) = self.line_starts.get(line - 1) else {
            return self.end;
        };
        let line_end = self.line_starts.get(line).map_or(self.origin.len(), |&next| next - 1);
        let i = (line_start + col.saturating_sub(1)).min(line_end);
        self.origin.get(i).copied().unwrap_or(self.end)
    }
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

/// Prefix every line of `sql` that is not whitespace-only with `indent`, the
/// inverse of the de-indentation in [`LintView::new`] for a literal whose SQL
/// starts on its own line.
pub fn reindent(sql: &str, indent: &str) -> String {
    sql.trim_end_matches('\n')
        .lines()
        .map(|l| if l.trim().is_empty() { l.to_string() } else { format!("{indent}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Re-indent `sql` for a literal whose first line is stuck to the opening
/// delimiter: the first line stays as is, and every other line that is not
/// whitespace-only is prefixed with `indent`, the inverse of [`LintView::new`]
/// for such a literal.
pub fn reindent_keep_first(sql: &str, indent: &str) -> String {
    let body = sql.trim_end_matches('\n');
    match body.split_once('\n') {
        Some((first, rest)) => format!("{first}\n{}", reindent(rest, indent)),
        None => body.to_string(),
    }
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
    fn content_matches_the_compiler_value() {
        let sources = [
            r##"r"SELECT 1""##,
            r###"r#"SELECT "a" FROM t"#"###,
            r####"r##"SELECT '"#' AS x"##"####,
            "r#\"SELECT a\r\n  FROM t\r\n\"#",
            r#""a\nb\rc\td\\e\0f\'g\"h""#,
            r#""\x41\x7f\x00""#,
            r#""\u{e9}\u{1F600}\u{10_FFFF}\u{0}""#,
            "\"SELECT c \\\n      FROM t\"",
            "\"SELECT c \\\r\n  \t\n   FROM t\"",
            "\"SELECT a\r\nFROM t\"",
            "\"SELECT 'é' AS \\\"日本\\\"\"",
            "\"multi\n    line\n\"",
            "\"\"",
        ];
        for text in sources {
            let value = syn::parse_str::<syn::LitStr>(text).unwrap().value();
            // syn keeps CRLF in raw strings; rustc normalizes it before lexing.
            let expected = if text.starts_with('r') { value.replace("\r\n", "\n") } else { value };
            assert_eq!(ParsedLiteral::parse(text).unwrap().content, expected, "{text:?}");
        }
    }

    #[test]
    fn only_quote_and_backslash_escapes_are_rewritable() {
        for text in ["r\"a\\nb\"", r#""a\"b\\c""#, "\"a\r\nb\"", "\"plain\""] {
            assert!(ParsedLiteral::parse(text).unwrap().rewritable, "{text:?}");
        }
        let others = [r#""a\nb""#, r#""\x41""#, r#""\u{e9}""#, r#""\'""#, r#""\t""#, "\"a \\\n  b\""];
        for text in others {
            assert!(!ParsedLiteral::parse(text).unwrap().rewritable, "{text:?}");
        }
    }

    #[test]
    fn records_crlf_line_endings() {
        assert!(ParsedLiteral::parse("\"a\r\nb\"").unwrap().crlf);
        assert!(ParsedLiteral::parse("r#\"a\r\nb\"#").unwrap().crlf);
        assert!(!ParsedLiteral::parse("\"a\nb\"").unwrap().crlf);
    }

    #[test]
    fn source_offsets_point_at_the_source_char() {
        let text = r#""a\"b\x41c\
            d""#;
        let lit = ParsedLiteral::parse(text).unwrap();
        assert_eq!(lit.content, "a\"bAcd");
        let offsets: Vec<usize> = (0..=lit.content.len()).map(|i| lit.source_offset(i)).collect();
        assert_eq!(offsets, [1, 2, 4, 5, 9, text.len() - 2, text.len() - 1]);
        let raw = ParsedLiteral::parse("r#\"a\r\nb\"#").unwrap();
        assert_eq!(raw.content, "a\nb");
        assert_eq!((0..4).map(|i| raw.source_offset(i)).collect::<Vec<_>>(), [3, 4, 6, 7]);
    }

    #[test]
    fn lint_view_dedents_only_continuation_lines_of_a_stuck_literal() {
        let c = "SELECT a\n        FROM t\n\n        WHERE x = 1";
        let view = LintView::new(c);
        assert_eq!(view.sql, "SELECT a\nFROM t\n\nWHERE x = 1\n");
        assert_eq!(view.content_index(1, 8), c.find('a').unwrap());
        assert_eq!(view.content_index(2, 1), c.find('F').unwrap());
        assert_eq!(view.content_index(4, 7), c.find('x').unwrap());
        // Past the end of a line, then past the end of the SQL.
        assert_eq!(view.content_index(2, 40), c.find("\n\n").unwrap());
        assert_eq!(view.content_index(9, 1), c.len());
    }

    #[test]
    fn content_index_treats_line_zero_as_the_first_line() {
        let c = "\n    SELECT a\n    FROM t\n";
        let view = LintView::new(c);
        assert_eq!(view.content_index(0, 0), view.content_index(1, 1));
        assert_eq!(view.content_index(0, 3), view.content_index(1, 3));
        assert_eq!(LintView::new("").content_index(0, 1), 0);
    }

    #[test]
    fn lint_view_maps_back_through_stripped_newlines_and_indent() {
        let c = "\n\n    SELECT a\n      FROM t\n    ";
        let view = LintView::new(c);
        assert_eq!(view.sql, "SELECT a\n  FROM t\n");
        assert_eq!(view.content_index(1, 1), c.find('S').unwrap());
        assert_eq!(view.content_index(2, 3), c.find('F').unwrap());
    }

    #[test]
    fn lint_view_strips_common_indent() {
        let c = "\n                SELECT\n                    a\n                FROM t\n                ";
        assert_eq!(LintView::new(c).sql, "SELECT\n    a\nFROM t\n");
    }

    #[test]
    fn lint_view_strips_only_the_common_whitespace_prefix() {
        let c = "\n\tSELECT 'x\n    y' AS a\n    ";
        let view = LintView::new(c);
        assert_eq!(view.indent, "");
        assert_eq!(view.sql, "\tSELECT 'x\n    y' AS a\n");
        let c = "\n    \t SELECT 1\n    \t   FROM t\n";
        let view = LintView::new(c);
        assert_eq!(view.indent, "    \t ");
        assert_eq!(view.sql, "SELECT 1\n  FROM t\n");
    }

    #[test]
    fn lint_view_keeps_whitespace_only_lines_as_written() {
        let c = "\n    SELECT 'a\n      \n  \n    b'\n";
        let view = LintView::new(c);
        assert_eq!(view.indent, "    ");
        assert_eq!(view.sql, "SELECT 'a\n      \n  \nb'\n");
        assert_eq!(view.content_index(2, 2), c.find("      ").unwrap() + 1);
        assert_eq!(view.content_index(3, 1), c.find("\n  \n").unwrap() + 1);
        assert_eq!(view.content_index(4, 1), c.find("b'").unwrap());
    }

    #[test]
    fn lint_view_keeps_whitespace_only_lines_shallower_than_the_indent() {
        let blank = ["    ", "\t  ", "        ", "            "];
        let lines = blank.join("\n");
        for (c, indent) in [
            (format!("\n        SELECT 'a\n{lines}\n        b' AS c\n        FROM t\n    "), "        "),
            (format!("SELECT 'a\n{lines}\n        b' AS c\n        FROM t"), "        "),
        ] {
            let view = LintView::new(&c);
            assert_eq!(view.indent, indent, "{c:?}");
            let sql: Vec<&str> = view.sql.lines().collect();
            assert_eq!(sql[1..5], blank, "{c:?}");
            for (n, line) in blank.iter().enumerate() {
                let at = c.find(&format!("\n{line}\n")).unwrap() + 1;
                assert_eq!(view.content_index(n + 2, 1), at, "{c:?}");
            }
        }
    }

    #[test]
    fn reindenting_the_lint_view_restores_the_content() {
        for c in [
            "\n        SELECT 'a\n          \n            b' AS c\n        FROM t\n    ",
            "\n\tSELECT 'x\n    y' AS a\n    ",
            "\n\n    \t SELECT 1\n\n    \t   FROM t\n",
            "SELECT 'p\n          \n     \t  q' AS b\n        FROM t",
            "SELECT a\n\tFROM t\n\tWHERE x = 1\n  ",
            "\n        SELECT 'a\n        \n   \n  \n          \n\n        b' AS c\n        ",
            "SELECT 'a\n        \n   \n  \n          \n\n        b' AS c\n        FROM t",
            "\n\n        \n        SELECT 1\n",
            "\n        SELECT 'a\n    \n\t  \n        \n            \n        b' AS c\n    ",
        ] {
            let view = LintView::new(c);
            let (leading, trailing) = framing(c);
            let body = if leading.is_empty() {
                reindent_keep_first(&view.sql, &view.indent)
            } else {
                reindent(&view.sql, &view.indent)
            };
            assert_eq!(format!("{leading}{body}{trailing}"), c);
        }
    }

    #[test]
    fn framing_block_style() {
        let c = "\n            SELECT 1\n            ";
        assert_eq!(LintView::new(c).indent, "            ");
        assert_eq!(framing(c), ("\n".to_string(), "\n            ".to_string()));
    }

    #[test]
    fn framing_inline_style_keeps_closing_stuck() {
        // `r#"INSERT ...\n   VALUES (...)"#`, no leading/trailing newline.
        let c = "INSERT INTO t\n           VALUES (1)";
        assert_eq!(framing(c), (String::new(), String::new()));
    }

    #[test]
    fn reindent_roundtrips() {
        let sql = "SELECT\n    a\nFROM t\n";
        assert_eq!(reindent(sql, "    "), "    SELECT\n        a\n    FROM t");
    }
}
