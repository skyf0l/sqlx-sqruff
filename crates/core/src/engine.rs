//! Per-file orchestration: check (lint → diagnostics) and fix (fix → writeback).

use crate::{
    diagnostic::Diagnostic,
    extract::extract,
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
pub fn check_file(path: &str, src: &str, engine: &SqruffEngine) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for q in extract(src) {
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

/// Fix every (multi-line raw, unless `all`) inline query and splice back.
/// Returns `None` for `new_src` if nothing changed; never returns a `.rs` that
/// fails to re-parse (safety invariant).
pub fn fix_file(
    path: &str,
    src: &str,
    engine: &SqruffEngine,
    only_multiline_raw: bool,
) -> Result<FixOutcome, String> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for q in extract(src) {
        let Some(lit) = ParsedLiteral::parse(&q.text) else {
            continue;
        };
        if only_multiline_raw && !(lit.is_raw() && lit.is_multiline()) {
            continue;
        }
        let sql = literal::dedent(&lit.content);
        if sql.trim().is_empty() || is_skippable(&sql) {
            continue;
        }
        let (fixed, _residual) = engine.fix(&sql);
        let new_literal = rebuild_literal(&lit, &fixed);
        if new_literal != q.text {
            edits.push((q.start_byte, q.end_byte, new_literal));
        }
    }

    if edits.is_empty() {
        return Ok(FixOutcome { new_src: None, queries_changed: 0 });
    }

    // Splice right-to-left so byte offsets stay valid.
    edits.sort_by(|a, b| b.0.cmp(&a.0));
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

/// Reassemble a raw-string literal around fixed SQL, preserving framing.
fn rebuild_literal(lit: &ParsedLiteral, fixed: &str) -> String {
    let LiteralKind::Raw { hashes } = lit.kind else {
        // Normal strings are only reached when only_multiline_raw is false; keep
        // them single-line by collapsing whitespace conservatively is risky, so
        // we re-emit as a raw multi-line literal only if the original was raw.
        // For normal literals we leave content as-is (no fix applied upstream).
        return rebuild_normal(lit, fixed);
    };
    let h: String = std::iter::repeat('#').take(hashes).collect();
    let indent = literal::block_indent(&lit.content);
    let body = literal::reindent(fixed, &indent);
    // Preserve the original framing verbatim so a no-op fix is a no-op diff and
    // the closing `"#` stays where the author put it (stuck, or on its own line).
    let (leading, trailing) = literal::framing(&lit.content);
    format!("r{h}\"{leading}{body}{trailing}\"{h}")
}

fn rebuild_normal(lit: &ParsedLiteral, fixed: &str) -> String {
    // Single-line normal string: re-escape minimally, keep one line.
    let one_line = fixed.trim().replace('\n', " ");
    let escaped = one_line.replace('\\', "\\\\").replace('"', "\\\"");
    let _ = lit;
    format!("\"{escaped}\"")
}

fn leading_newlines(content: &str) -> usize {
    content.chars().take_while(|&c| c == '\n').count()
}

/// `SET custom.guc = …` config statements are not queries.
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
        let first = fix_file("f.rs", src, &eng, true).unwrap();
        let fixed = first.new_src.expect("should change");
        assert!(fixed.contains("IS NULL"));
        let second = fix_file("f.rs", &fixed, &eng, true).unwrap();
        assert!(second.new_src.is_none(), "second run must be a no-op");
    }

    #[test]
    fn fix_preserves_placeholders_and_casts() {
        let src = "fn f(){\n    let _=sqlx::query_as!(Row, r#\"\n        SELECT a AS \"a!: T\" FROM t WHERE x = NULL AND y = $1\n    \"#, p);\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng, true).unwrap().new_src.unwrap();
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
        let out = fix_file("f.rs", src, &eng, true).unwrap();
        assert!(out.new_src.is_none(), "no-op fix must not rewrite framing");
    }

    #[test]
    fn skips_set_statements() {
        assert!(is_skippable("SET audit.skip = 'true'\n"));
        assert!(!is_skippable("SELECT 1\n"));
    }
}
