//! Locate `sqlx::query*!` macros and the byte span of their SQL string-literal
//! argument, using `syn` + `proc-macro2` span locations.

use proc_macro2::{LineColumn, Literal, TokenTree};
use syn::visit::Visit;

/// One inline SQL literal located in a source file.
#[derive(Debug, Clone)]
pub struct QueryLiteral {
    /// Byte range of the literal token (including delimiters) in the source.
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line where the literal token starts (for diagnostics).
    pub line: usize,
    /// Exact source text of the literal, e.g. `r#"SELECT 1"#`.
    pub text: String,
}

const QUERY_MACROS: &[&str] = &[
    "query",
    "query_as",
    "query_scalar",
    "query_unchecked",
    "query_as_unchecked",
    "query_scalar_unchecked",
];

/// Extract every inline query literal. Returns empty (not an error) if the file
/// does not parse; use [`extract_checked`] to distinguish "no queries" from
/// "file didn't parse" so callers can warn instead of silently skipping.
pub fn extract(src: &str) -> Vec<QueryLiteral> {
    extract_checked(src).unwrap_or_default()
}

/// Like [`extract`], but returns the `syn` parse error so callers can warn that
/// a whole file was skipped (rather than hiding its queries).
pub fn extract_checked(src: &str) -> Result<Vec<QueryLiteral>, String> {
    // Cheap byte-substring gate before the expensive `syn::parse_file`: every
    // supported macro name (`query`, `query_as`, ...) contains "query", so a file
    // without that substring has no inline SQL and need not be parsed at all.
    // In a typical codebase most `.rs` files hold no `query*!` macro, so this
    // skips the bulk of the parsing work. A file that fails to parse but
    // contains no "query" is reported as having no queries (Ok-empty), never as
    // skipped — which is the correct outcome.
    if !src.contains("query") {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(src).map_err(|e| e.to_string())?;
    let mut v = Visitor { found: Vec::new() };
    v.visit_file(&file);

    let index = LineIndex::new(src);
    Ok(v.found
        .into_iter()
        .filter_map(|lit| {
            let span = lit.span();
            let start_byte = index.offset(src, span.start())?;
            let end_byte = index.offset(src, span.end())?;
            let text = src.get(start_byte..end_byte)?.to_string();
            Some(QueryLiteral { start_byte, end_byte, line: span.start().line, text })
        })
        .collect())
}

struct Visitor {
    found: Vec<Literal>,
}

impl<'ast> Visit<'ast> for Visitor {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if is_query_macro(&m.path) {
            if let Some(lit) = first_string_literal(m.tokens.clone()) {
                self.found.push(lit);
            }
        }
        syn::visit::visit_macro(self, m);
    }
}

fn is_query_macro(path: &syn::Path) -> bool {
    match path.segments.last() {
        Some(seg) => QUERY_MACROS.contains(&seg.ident.to_string().as_str()),
        None => false,
    }
}

/// First string literal in the token stream; naturally skips a leading struct
/// ident + comma (`query_as!(Row, r#"..."#)`) and stops before bind-arg literals.
fn first_string_literal(tokens: proc_macro2::TokenStream) -> Option<Literal> {
    for tt in tokens {
        if let TokenTree::Literal(l) = tt {
            let s = l.to_string();
            if s.starts_with('"') || s.starts_with("r\"") || s.starts_with("r#") {
                return Some(l);
            }
        }
    }
    None
}

/// Maps `proc_macro2` (line, column) to a byte offset in the source.
struct LineIndex {
    /// Byte offset of the start of each line (line N at index N-1).
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(src: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    /// `LineColumn`: 1-based line, 0-based column counted in `char`s.
    fn offset(&self, src: &str, lc: LineColumn) -> Option<usize> {
        let line_start = *self.line_starts.get(lc.line.checked_sub(1)?)?;
        let rest = src.get(line_start..)?;
        let mut byte = line_start;
        for (k, ch) in rest.chars().enumerate() {
            if k == lc.column {
                return Some(byte);
            }
            byte += ch.len_utf8();
        }
        Some(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_query_as_with_leading_ident() {
        let src = r###"
fn f() {
    let _ = sqlx::query_as!(Row, r#"SELECT 1"#, x);
}
"###;
        let q = extract(src);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].text, "r#\"SELECT 1\"#");
    }

    #[test]
    fn finds_bare_query_and_scalar() {
        let src = r###"
fn f() {
    let _ = query!("SELECT 1");
    let _ = sqlx::query_scalar!(r#"SELECT 2"#);
}
"###;
        let q = extract(src);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].text, "\"SELECT 1\"");
        assert_eq!(q[1].text, "r#\"SELECT 2\"#");
    }

    #[test]
    fn byte_span_is_exact() {
        let src = "fn f(){let _=query!(r#\"SELECT 1\"#);}";
        let q = extract(src);
        assert_eq!(q.len(), 1);
        assert_eq!(&src[q[0].start_byte..q[0].end_byte], "r#\"SELECT 1\"#");
    }

    #[test]
    fn ignores_unparsable_file() {
        assert!(extract("this is not rust {{{").is_empty());
    }

    #[test]
    fn pre_filter_skips_query_free_files_without_parsing() {
        // No "query" substring → Ok-empty without invoking syn, even if the file
        // would not parse. Such files are never reported as skipped/unparsable.
        assert!(extract_checked("this is not rust {{{").unwrap().is_empty());
        assert!(extract_checked("fn f() { let x = 1; }").unwrap().is_empty());
    }

    #[test]
    fn pre_filter_still_errors_on_unparsable_file_with_query() {
        // Contains "query" → syn must still run, so a genuine parse failure is
        // surfaced (the CLI turns this into a skip warning).
        assert!(extract_checked("query! this is not rust {{{").is_err());
    }
}
