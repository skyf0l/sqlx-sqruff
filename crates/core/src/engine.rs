//! Per-file orchestration: check (lint → diagnostics) and fix (fix → writeback).

use std::cmp::Reverse;

use crate::{
    diagnostic::Diagnostic,
    extract::{QueryLiteral, extract},
    literal::{self, LiteralKind, ParsedLiteral},
    sqruff_adapter::SqruffEngine,
};

/// Debug: return each query's `(line, dedented_sql)` as the linter sees it.
pub fn dump_file(src: &str) -> Vec<(usize, String)> {
    extract(src)
        .into_iter()
        .filter_map(|q| {
            let lit = ParsedLiteral::parse(&q.text)?;
            Some((q.line, literal::dedent(&lit.content)))
        })
        .collect()
}

/// Lint every inline query in `src`; map findings to `.rs` locations.
/// Convenience wrapper that extracts then lints (used by tests). The CLI calls
/// [`extract_checked`](crate::extract::extract_checked) once and then
/// [`check_extracted`] directly, to avoid parsing each file twice.
pub fn check_file(path: &str, src: &str, engine: &SqruffEngine) -> Vec<Diagnostic> {
    check_extracted(path, &extract(src), engine)
}

/// Lint pre-extracted queries; map findings to `.rs` locations.
pub fn check_extracted(
    path: &str,
    queries: &[QueryLiteral],
    engine: &SqruffEngine,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for q in queries {
        let Some(lit) = ParsedLiteral::parse(&q.text) else {
            continue;
        };
        let sql = literal::dedent(&lit.content);
        if sql.trim().is_empty() || is_skippable(&sql) {
            continue;
        }
        let lead_newlines = leading_newlines(&lit.content);
        let indent_len = literal::block_indent(&lit.content).chars().count();
        for f in engine.lint(&sql) {
            diags.push(Diagnostic {
                file: path.to_string(),
                // SQL line N → .rs line = literal-start line + stripped leading
                // newlines + (N - 1).
                line: q.line + lead_newlines + f.line.saturating_sub(1),
                // dedent removed `indent_len` cols; col is 0-based in SQL.
                col: f.col + indent_len + 1,
                code: f.code,
                message: f.desc,
            });
        }
    }
    diags
}

pub struct FixOutcome {
    pub new_src: Option<String>,
    pub queries_changed: usize,
}

/// Fix every inline query and splice back, preserving each literal's line-shape
/// (one-liners stay one line, multi-line raw blocks keep block layout).
/// Returns `None` for `new_src` if nothing changed; never returns a `.rs` that
/// fails to re-parse (safety invariant).
///
/// Convenience wrapper that extracts then fixes (used by tests). The CLI calls
/// [`extract_checked`](crate::extract::extract_checked) once and then
/// [`fix_extracted`] directly, to avoid parsing each file twice.
pub fn fix_file(path: &str, src: &str, engine: &SqruffEngine) -> Result<FixOutcome, String> {
    fix_extracted(path, src, &extract(src), engine)
}

/// Fix pre-extracted queries and splice back into `src`.
pub fn fix_extracted(
    path: &str,
    src: &str,
    queries: &[QueryLiteral],
    engine: &SqruffEngine,
) -> Result<FixOutcome, String> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for q in queries {
        let Some(lit) = ParsedLiteral::parse(&q.text) else {
            continue;
        };
        let sql = literal::dedent(&lit.content);
        if sql.trim().is_empty() || is_skippable(&sql) {
            continue;
        }
        let (fixed, _residual) = engine.fix(&sql);
        if let Some(new_literal) = rebuild_literal(&lit, &fixed)
            && new_literal != q.text
        {
            edits.push((q.start_byte, q.end_byte, new_literal));
        }
    }

    if edits.is_empty() {
        return Ok(FixOutcome { new_src: None, queries_changed: 0 });
    }

    // Splice right-to-left so byte offsets stay valid.
    edits.sort_by_key(|&(start, ..)| Reverse(start));
    let mut new_src = src.to_string();
    for (start, end, repl) in &edits {
        new_src.replace_range(start..end, repl);
    }

    // Safety invariant: never emit a file that no longer parses.
    if syn::parse_file(&new_src).is_err() {
        return Err(format!("fix produced unparsable Rust in {path}; discarded"));
    }

    Ok(FixOutcome { new_src: Some(new_src), queries_changed: edits.len() })
}

/// Reassemble a literal around fixed SQL, preserving its line-shape.
///
/// Line-shape is the author's choice, keyed off the *current* literal: a
/// multi-line raw block keeps block layout (reindent + preserve framing), so the
/// way to opt a query into block formatting is to add newlines to its raw
/// string. Everything else (single-line raw and all normal literals) stays on
/// one line. Returns `None` when a one-line rebuild would be unsafe (a fix folded
/// a `--` comment across lines).
fn rebuild_literal(lit: &ParsedLiteral, fixed: &str) -> Option<String> {
    if let LiteralKind::Raw { hashes } = lit.kind
        && lit.is_multiline()
    {
        let h = "#".repeat(hashes);
        let indent = literal::block_indent(&lit.content);
        let body = literal::reindent(fixed, &indent);
        // Preserve the original framing verbatim so a no-op fix is a no-op diff
        // and the closing `"#` stays where the author put it (stuck, or on its
        // own line).
        let (leading, trailing) = literal::framing(&lit.content);
        return Some(format!("r{h}\"{leading}{body}{trailing}\"{h}"));
    }
    rebuild_oneline(lit, fixed)
}

/// Collapse fixed SQL onto one line, re-emitting in the literal's own delimiter
/// (raw keeps its hashes; normal re-escapes). `None` when collapsing would fold a
/// `--` line comment into following code; that one-liner is left untouched.
fn rebuild_oneline(lit: &ParsedLiteral, fixed: &str) -> Option<String> {
    let lines: Vec<&str> = fixed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() > 1 && fixed.contains("--") {
        return None;
    }
    let one = lines.join(" ");
    Some(match lit.kind {
        LiteralKind::Raw { hashes } => {
            let h = "#".repeat(hashes);
            format!("r{h}\"{one}\"{h}")
        }
        LiteralKind::Normal => {
            let escaped = one.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    })
}

fn leading_newlines(content: &str) -> usize {
    content.chars().take_while(|&c| c == '\n').count()
}

/// `SET custom.guc = ...` config statements are not queries.
fn is_skippable(sql: &str) -> bool {
    let t = sql.trim_start();
    t.len() >= 4 && t[..4].eq_ignore_ascii_case("SET ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> SqruffEngine {
        SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = CV05,ST01,ST02\n")
            .expect("engine")
    }

    #[test]
    fn check_finds_is_null_misuse() {
        let src = "fn f(){let _=sqlx::query!(r#\"\n    SELECT a FROM t WHERE x = NULL\n    \"#);}";
        let eng =
            SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = CV05\n").unwrap();
        let diags = check_file("f.rs", src, &eng);
        assert!(diags.iter().any(|d| d.code == "CV05"));
    }

    #[test]
    fn fix_is_idempotent() {
        let src = "fn f(){\n    let _=sqlx::query!(r#\"\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n}\n";
        let eng = engine();
        let first = fix_file("f.rs", src, &eng).unwrap();
        let fixed = first.new_src.expect("should change");
        assert!(fixed.contains("IS NULL"));
        let second = fix_file("f.rs", &fixed, &eng).unwrap();
        assert!(second.new_src.is_none(), "second run must be a no-op");
    }

    #[test]
    fn fix_preserves_placeholders_and_casts() {
        let src = "fn f(){\n    let _=sqlx::query_as!(Row, r#\"\n        SELECT a AS \"a!: T\" FROM t WHERE x = NULL AND y = $1\n    \"#, p);\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.unwrap();
        assert!(out.contains("$1"));
        assert!(out.contains("\"a!: T\""));
        assert!(out.contains("IS NULL"));
    }

    #[test]
    fn inline_style_noop_is_byte_identical() {
        // SQL stuck to opening `r#"` and closing `"#` stuck to last line; safe-fix
        // has nothing to change → output must equal input (no added newline).
        let src = "fn f(){\n    sqlx::query!(r#\"INSERT INTO t\n           (a, b)\n           VALUES ($1, $2)\"#, x, y);\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert!(out.new_src.is_none(), "no-op fix must not rewrite framing");
    }

    #[test]
    fn fixes_single_line_raw_in_place() {
        // A one-line raw literal (raw only because of the `"` cast) gets fixed
        // but must stay on one line, not exploded into a block.
        let src =
            "fn f(){\n    let _=sqlx::query!(r#\"SELECT a FROM t WHERE x = NULL\"#);\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        // fixed AND still on one line (no newline inside the raw delimiters).
        assert!(out.contains("r#\"SELECT a FROM t WHERE x IS NULL\"#"), "got: {out}");
    }

    #[test]
    fn fixes_single_line_normal_in_place() {
        let src = "fn f(){\n    let _=sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(out.contains("\"SELECT a FROM t WHERE x IS NULL\""), "got: {out}");
    }

    #[test]
    fn adding_a_newline_opts_into_block_layout() {
        // Author opts a query into block formatting by writing it multi-line.
        let src = "fn f(){\n    let _=sqlx::query!(r#\"\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(out.contains("IS NULL"));
        // stays a multi-line block (opening `r#"` then a newline).
        assert!(out.contains("r#\"\n"), "got: {out}");
    }

    #[test]
    fn skips_set_statements() {
        assert!(is_skippable("SET audit.skip = 'true'\n"));
        assert!(!is_skippable("SELECT 1\n"));
    }
}
