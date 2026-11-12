//! Per-file orchestration: check (lint -> diagnostics) and fix (fix -> writeback).

use std::cmp::Reverse;

use crate::{
    diagnostic::Diagnostic,
    extract::{QueryLiteral, extract},
    line_index::LineIndex,
    literal::{self, LintView, LiteralKind, ParsedLiteral},
    sqruff_adapter::SqruffEngine,
};

/// Debug: return each pre-extracted query's `(line, sql)` as the linter sees it.
pub fn dump_extracted(queries: &[QueryLiteral]) -> Vec<(usize, String)> {
    queries
        .iter()
        .filter_map(|q| {
            let lit = ParsedLiteral::parse(&q.text)?;
            Some((q.line, LintView::new(&lit.content).sql))
        })
        .collect()
}

/// Lint every inline query in `src`; map findings to `.rs` locations.
/// Convenience wrapper that extracts then lints (used by tests). The CLI calls
/// [`extract_checked`](crate::extract::extract_checked) once and then
/// [`check_extracted`] directly, to avoid parsing each file twice.
pub fn check_file(path: &str, src: &str, engine: &SqruffEngine) -> Vec<Diagnostic> {
    check_extracted(path, src, &extract(src), engine)
}

/// Lint pre-extracted queries of `src`; map findings to `.rs` locations.
pub fn check_extracted(
    path: &str,
    src: &str,
    queries: &[QueryLiteral],
    engine: &SqruffEngine,
) -> Vec<Diagnostic> {
    let index = LineIndex::new(src);
    let mut diags = Vec::new();
    for q in queries {
        let Some(lit) = ParsedLiteral::parse(&q.text) else {
            continue;
        };
        let view = LintView::new(&lit.content);
        if view.sql.trim().is_empty() || is_skippable(&view.sql) {
            continue;
        }
        let findings = match engine.lint(&view.sql) {
            Ok(findings) => findings,
            Err(e) => {
                let (line, col) = index.line_col(src, q.start_byte + lit.source_offset(0));
                diags.push(Diagnostic {
                    file: path.to_string(),
                    line,
                    col,
                    code: "ERROR".to_string(),
                    message: first_line(&e).to_string(),
                });
                continue;
            }
        };
        for f in findings {
            let at = q.start_byte + lit.source_offset(view.content_index(f.line, f.col));
            let (line, col) = index.line_col(src, at);
            diags.push(Diagnostic {
                file: path.to_string(),
                line,
                col,
                code: f.code,
                message: f.desc,
            });
        }
    }
    diags
}

/// One literal that `fix` rewrites: its 1-based `.rs` line and its source text
/// before and after.
#[derive(Debug, Clone)]
pub struct QueryChange {
    pub line: usize,
    pub before: String,
    pub after: String,
}

pub struct FixOutcome {
    pub new_src: Option<String>,
    /// The rewritten literals, in source order.
    pub changes: Vec<QueryChange>,
    /// Literals that needed a fix but were left untouched because they use
    /// escapes other than `\"` and `\\`, which the rewrite cannot reproduce.
    pub skipped_escapes: usize,
    /// 1-based `.rs` lines of literals whose rewritten text would not have the
    /// fixed SQL as its value; those fixes were not applied.
    pub dropped_lines: Vec<usize>,
    /// 1-based `.rs` line and first line of sqruff's error message for each
    /// literal sqruff could not process; those literals were left untouched.
    pub sqruff_errors: Vec<(usize, String)>,
    /// 1-based `.rs` lines of literals that sqruff changed but whose literal
    /// could not be rewritten in its current line layout (for example a
    /// one-liner whose fix needs line breaks); those fixes were not applied.
    pub layout_kept_lines: Vec<usize>,
    /// 1-based `.rs` lines of literals whose SQL did not fully parse with the
    /// configured dialect; sqruff's fixes for them were not applied, because a
    /// fix built on a wrong parse can change what the query does.
    pub unparsable_lines: Vec<usize>,
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
    let mut changes = Vec::new();
    let mut skipped_escapes = 0;
    let mut dropped_lines = Vec::new();
    let mut sqruff_errors = Vec::new();
    let mut layout_kept_lines = Vec::new();
    let mut unparsable_lines = Vec::new();
    for q in queries {
        let Some(lit) = ParsedLiteral::parse(&q.text) else {
            continue;
        };
        let view = LintView::new(&lit.content);
        if view.sql.trim().is_empty() || is_skippable(&view.sql) {
            continue;
        }
        let (fixed, residual) = match engine.fix(&view.sql) {
            Ok(result) => result,
            Err(e) => {
                sqruff_errors.push((q.line, first_line(&e).to_string()));
                continue;
            }
        };
        if fixed == view.sql {
            continue;
        }
        if residual.iter().any(|f| f.unparsable) {
            unparsable_lines.push(q.line);
            continue;
        }
        if !lit.rewritable {
            skipped_escapes += 1;
            continue;
        }
        let Some(mut content) = rebuild_content(&lit, &view.indent, &fixed) else {
            layout_kept_lines.push(q.line);
            continue;
        };
        if lit.crlf {
            content = content.replace('\n', "\r\n");
        }
        let new_literal = wrap(&lit, &content);
        if new_literal == q.text {
            layout_kept_lines.push(q.line);
            continue;
        }
        if !has_value(&new_literal, &content) {
            dropped_lines.push(q.line);
            continue;
        }
        changes.push(QueryChange { line: q.line, before: q.text.clone(), after: new_literal.clone() });
        edits.push((q.start_byte, q.end_byte, new_literal));
    }

    if edits.is_empty() {
        return Ok(FixOutcome {
            new_src: None,
            changes,
            skipped_escapes,
            dropped_lines,
            sqruff_errors,
            layout_kept_lines,
            unparsable_lines,
        });
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

    Ok(FixOutcome {
        new_src: Some(new_src),
        changes,
        skipped_escapes,
        dropped_lines,
        sqruff_errors,
        layout_kept_lines,
        unparsable_lines,
    })
}

/// Whether `literal` parses as a single string literal whose value is `content`.
fn has_value(literal: &str, content: &str) -> bool {
    // rustc turns CRLF into LF before lexing; syn does not do so in raw strings.
    syn::parse_str::<syn::LitStr>(literal)
        .is_ok_and(|l| l.value().replace("\r\n", "\n") == content.replace("\r\n", "\n"))
}

/// The new content of a literal around fixed SQL, preserving its line-shape.
///
/// Line-shape is the author's choice, keyed off whether the *current* literal
/// already spans multiple lines: a multi-line literal keeps block layout
/// (reindent + preserve framing), a one-liner stays on one line. This is
/// independent of the delimiter kind, since multi-line SQL is common in both raw
/// (`r#"..."#`) and plain `"..."` strings; [`wrap`] then preserves the kind (raw
/// keeps its hashes, normal re-escapes). `indent` is the [`LintView::indent`]
/// the SQL was linted with. Returns `None` when a one-line rebuild would be
/// unsafe (a fix folded a `--` comment across lines).
fn rebuild_content(lit: &ParsedLiteral, indent: &str, fixed: &str) -> Option<String> {
    if lit.is_multiline() {
        // Preserve the original framing verbatim so a no-op fix is a no-op diff
        // and the closing delimiter stays where the author put it (stuck, or on
        // its own line).
        let (leading, trailing) = literal::framing(&lit.content);
        let body = if leading.is_empty() {
            // First SQL line is stuck to the opening delimiter (`"UPDATE ...`), so
            // it carries no indent of its own. Keep it flush and align the
            // continuation lines to the indent the author gave them, rather than
            // letting the common indent collapse to 0.
            literal::reindent_keep_first(fixed, indent)
        } else {
            // Leading newline: every SQL line sits on its own line, so the common
            // block indent re-applies cleanly to all of them.
            literal::reindent(fixed, indent)
        };
        return Some(format!("{leading}{body}{trailing}"));
    }
    rebuild_oneline(fixed)
}

/// Collapse fixed SQL onto one line. `None` when collapsing would fold a `--`
/// line comment into following code; that one-liner is left untouched.
fn rebuild_oneline(fixed: &str) -> Option<String> {
    let lines: Vec<&str> = fixed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() > 1 && fixed.contains("--") {
        return None;
    }
    let mut out = String::new();
    for line in lines {
        let tight = out.ends_with(['(', '[']) || line.starts_with([')', ']']);
        if !out.is_empty() && !tight {
            out.push(' ');
        }
        out.push_str(line);
    }
    Some(out)
}

/// Wrap inner SQL in the literal's own delimiter: raw keeps its hashes, normal
/// re-escapes `\` and `"`. Literal newlines in `inner` are left intact for both
/// (valid in raw and in multi-line normal strings alike).
fn wrap(lit: &ParsedLiteral, inner: &str) -> String {
    match lit.kind {
        LiteralKind::Raw { hashes } => {
            let h = "#".repeat(hashes);
            format!("r{h}\"{inner}\"{h}")
        }
        LiteralKind::Normal => {
            let escaped = inner.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

/// `SET custom.guc = ...` config statements are not queries.
fn is_skippable(sql: &str) -> bool {
    sql.trim_start().get(..4).is_some_and(|start| start.eq_ignore_ascii_case("SET "))
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
        // has nothing to change -> output must equal input (no added newline).
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
    fn too_long_one_liner_is_not_padded_inside_brackets() {
        // sqruff wraps a line longer than LT05 allows; collapsing it back onto one
        // line must not leave the wrap's line breaks as spaces inside brackets.
        let src = "fn f() {\n    let _ = sqlx::query(\"CREATE TEMPORARY TABLE users (id INTEGER GENERATED ALWAYS AS IDENTITY, tags TEXT[] NOT NULL)\");\n}\n";
        let eng =
            SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = LT01,LT05\n").unwrap();
        let out = fix_file("f.rs", src, &eng).unwrap();
        let fixed = out.new_src.as_deref().unwrap_or(src);
        assert!(!check_file("f.rs", fixed, &eng).iter().any(|d| d.code == "LT01"), "got: {fixed}");
        assert!(fix_file("f.rs", fixed, &eng).unwrap().new_src.is_none(), "second fix changed: {fixed}");
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
    fn multiline_normal_string_stays_multiline() {
        // A hand-formatted multi-line *normal* `"..."` string (no `r#`) must keep
        // its block layout, not get collapsed onto one line.
        let src = "fn f(){\n    let _=sqlx::query!(\n        \"SELECT a\n        FROM t\n        WHERE x = NULL\",\n        p,\n    );\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(out.contains("IS NULL"));
        // still spans multiple lines (a newline survives inside the literal).
        assert!(out.matches('\n').count() >= 6, "collapsed to one line: {out}");
    }

    #[test]
    fn stuck_first_line_keeps_continuation_indent() {
        // First SQL line stuck to the opening quote; continuation lines indented
        // 8 spaces. After fixing, that 8-space alignment must be preserved (not
        // collapsed to column 0).
        let src = "fn f(){\n    let _=sqlx::query!(\n        \"SELECT a\n        FROM t\n        WHERE x = NULL\",\n        p,\n    );\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(out.contains("        FROM t"), "lost continuation indent: {out}");
        assert!(out.contains("        WHERE x IS NULL"), "lost continuation indent: {out}");
    }

    #[test]
    fn multiline_normal_fix_is_idempotent() {
        let src = "fn f(){\n    let _=sqlx::query!(\n        \"SELECT a\n        FROM t\n        WHERE x = NULL\",\n        p,\n    );\n}\n";
        let eng = engine();
        let fixed = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        let second = fix_file("f.rs", &fixed, &eng).unwrap();
        assert!(second.new_src.is_none(), "second run must be a no-op");
    }

    #[test]
    fn fixes_function_call_sql_and_leaves_bind_literals_alone() {
        let src = "fn f(){\n    let _=sqlx::query(\"SELECT a FROM t WHERE x = NULL\").bind(\"x = NULL\");\n}\n";
        let eng = engine();
        let out = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(out.contains("sqlx::query(\"SELECT a FROM t WHERE x IS NULL\")"), "got: {out}");
        assert!(out.contains(".bind(\"x = NULL\")"), "bind literal was rewritten: {out}");
    }

    fn layout_engine() -> SqruffEngine {
        SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = LT01,LT02,CV05,CP01\n")
            .expect("engine")
    }

    #[test]
    fn fix_leaves_literals_with_other_escapes_untouched() {
        let src = r###"fn f() {
    let _ = sqlx::query("SELECT '\x41' AS a FROM t WHERE x = NULL");
    let _ = sqlx::query("SELECT '\u{e9}' AS b FROM t WHERE x = NULL");
    let _ = sqlx::query("SELECT c \
                         FROM t WHERE x = NULL");
    let _ = sqlx::query("SELECT a\nFROM t WHERE x = NULL");
}
"###;
        let out = fix_file("f.rs", src, &engine()).unwrap();
        assert_eq!(out.new_src, None);
        assert_eq!(out.skipped_escapes, 4);
    }

    #[test]
    fn check_reads_a_line_continuation_as_one_line() {
        let src = "fn f() {\n    let _ = sqlx::query(\"SELECT c \\\n                         FROM t\");\n}\n";
        let diags = check_file("f.rs", src, &layout_engine());
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn fix_drops_an_edit_that_would_change_the_literal_value() {
        // Quoting identifiers puts a `"` inside `r"..."`, which would end the raw
        // string early.
        let eng = SqruffEngine::from_source(
            "[sqruff]\ndialect = postgres\nrules = RF06\n\n\
             [sqruff:rules:references.quoting]\nprefer_quoted_identifiers = True\n",
        )
        .unwrap();
        let src = "fn f() {\n    let _ = sqlx::query!(r\"SELECT a FROM t\");\n    let _ = sqlx::query!(\"SELECT b FROM u\");\n}\n";
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert_eq!(out.dropped_lines, [2]);
        let new_src = out.new_src.expect("the normal literal is fixed");
        assert!(new_src.contains("r\"SELECT a FROM t\""), "{new_src}");
        assert!(new_src.contains(r#""SELECT \"b\" FROM \"u\"""#), "{new_src}");
    }

    #[test]
    fn fix_keeps_crlf_line_endings_inside_rebuilt_literals() {
        let src = "fn f() {\r\n    let _ = sqlx::query!(r#\"\r\n        SELECT a\r\n        FROM t WHERE x = NULL\r\n    \"#);\r\n    let _ = sqlx::query!(\"SELECT b\r\n        FROM u\r\n        WHERE y = NULL\");\r\n}\r\n";
        let eng = engine();
        let fixed = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(!fixed.replace("\r\n", "").contains('\n'), "bare LF in:\n{fixed:?}");
        assert!(fixed.contains("x IS NULL") && fixed.contains("y IS NULL"), "{fixed:?}");
        let again = fix_file("f.rs", &fixed, &eng).unwrap();
        assert_eq!(again.new_src, None, "second fix changed:\n{fixed:?}");
    }

    /// An inline config line naming an unknown dialect makes sqruff return an
    /// error instead of a lint result.
    const SQRUFF_ERROR_SRC: &str = "fn f() {\n    let _ = sqlx::query!(r#\"\n        -- sqruff:dialect:not_a_dialect\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n    let _ = sqlx::query!(\"SELECT b FROM u WHERE y = NULL\");\n}\n";

    #[test]
    fn check_reports_a_sqruff_error_at_the_literal() {
        let diags = check_file("f.rs", SQRUFF_ERROR_SRC, &engine());
        let errors: Vec<_> = diags.iter().filter(|d| d.code == "ERROR").collect();
        assert_eq!(errors.len(), 1, "{diags:?}");
        assert_eq!((errors[0].line, errors[0].col), (2, 29));
        assert!(!errors[0].message.is_empty() && !errors[0].message.contains('\n'), "{errors:?}");
        assert!(diags.iter().any(|d| d.code == "CV05" && d.line == 6), "{diags:?}");
    }

    #[test]
    fn fix_leaves_a_literal_sqruff_cannot_process_untouched() {
        let out = fix_file("f.rs", SQRUFF_ERROR_SRC, &engine()).unwrap();
        assert_eq!(out.sqruff_errors.iter().map(|(line, _)| *line).collect::<Vec<_>>(), [2]);
        let new_src = out.new_src.expect("the other literal is fixed");
        assert!(new_src.contains("x = NULL"), "{new_src}");
        assert!(new_src.contains("y IS NULL"), "{new_src}");
    }

    fn positions(src: &str) -> Vec<(usize, usize, String)> {
        let eng =
            SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = CV05\n").unwrap();
        check_file("f.rs", src, &eng).into_iter().map(|d| (d.line, d.col, d.code)).collect()
    }

    #[test]
    fn position_in_one_line_normal_literal() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT FROM WHERE ((\");\n}\n";
        // sqruff reports the unclosed bracket at the second `(`.
        assert_eq!(positions(src), [(2, 46, "????".to_string())]);
    }

    #[test]
    fn position_in_one_line_raw_literal() {
        let src = "fn f() {\n    let _ = sqlx::query!(r#\"SELECT a FROM t WHERE x = NULL\"#);\n}\n";
        assert_eq!(positions(src), [(2, 53, "CV05".to_string())]);
    }

    #[test]
    fn position_in_raw_block_literal() {
        let src = "fn f() {\n    let _ = sqlx::query!(r#\"\n        SELECT a\n        FROM t\n        WHERE x = NULL\n    \"#);\n}\n";
        assert_eq!(positions(src), [(5, 17, "CV05".to_string())]);
    }

    #[test]
    fn position_in_stuck_first_line_literal() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a FROM t WHERE x = NULL\n        AND y = NULL\");\n}\n";
        assert_eq!(positions(src), [(2, 51, "CV05".to_string()), (3, 15, "CV05".to_string())]);
    }

    #[test]
    fn position_after_escaped_quotes() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT \\\"a\\\" FROM t WHERE x = NULL\");\n}\n";
        assert_eq!(positions(src), [(2, 55, "CV05".to_string())]);
    }

    #[test]
    fn position_after_newline_escape_stays_on_the_source_line() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a\\nFROM t WHERE x = NULL\");\n}\n";
        assert_eq!(positions(src), [(2, 52, "CV05".to_string())]);
    }

    #[test]
    fn stuck_first_line_layout_is_clean_after_fix() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a\n        FROM t\n        WHERE x = 1\");\n}\n";
        let eng = layout_engine();
        let fixed = fix_file("f.rs", src, &eng).unwrap().new_src.unwrap_or_else(|| src.to_string());
        let diags = check_file("f.rs", &fixed, &eng);
        assert!(diags.is_empty(), "{diags:?}\n{fixed}");
    }

    #[test]
    fn fix_converges_with_check_for_every_literal_shape() {
        let fixtures = [
            "fn f() {\n    let _ = sqlx::query!(r#\"select  a from t where x = null\"#);\n}\n",
            "fn f() {\n    let _ = sqlx::query(\"select  a from t where x = null\");\n}\n",
            "fn f() {\n    let _ = sqlx::query!(r#\"\n        select  a\n            from t where x = null\n    \"#);\n}\n",
            "fn f() {\n    let _ = sqlx::query!(\"SELECT a\n        FROM t\n        WHERE x = 1\");\n}\n",
            "fn f() {\n    let _ = sqlx::query!(\"select  a\n            from t\n        where x = null\",\n    );\n}\n",
            "fn f() {\n    let _ = sqlx::query!(r#\"SELECT a AS \"a!\"\n        FROM t\n          WHERE x = NULL\"#);\n}\n",
            "fn f() {\n    let _ = sqlx::query(\"SELECT \\\"a\\\"  FROM t WHERE x = NULL\");\n}\n",
            "fn f() {\n    let _ = sqlx::query(\n        \"SELECT \\\"a\\\"\n         FROM t\n         WHERE x = NULL\",\n    );\n}\n",
            "fn f() {\n    let _ = sqlx::query!(\n        r#\"\n        insert into notes (body) values ('a\n        \n   \n  \n          \n        b'),  ('x')\n        \"#\n    );\n}\n",
        ];
        let eng = layout_engine();
        for src in fixtures {
            let fixed =
                fix_file("f.rs", src, &eng).unwrap().new_src.unwrap_or_else(|| src.to_string());
            let again = fix_file("f.rs", &fixed, &eng).unwrap();
            assert_eq!(again.new_src, None, "second fix changed:\n{fixed}");
            for q in extract(&fixed) {
                let lit = ParsedLiteral::parse(&q.text).unwrap();
                let fixable: Vec<_> = eng
                    .lint(&LintView::new(&lit.content).sql)
                    .unwrap()
                    .into_iter()
                    .filter(|f| f.fixable)
                    .map(|f| f.code)
                    .collect();
                assert!(fixable.is_empty(), "{fixable:?} left in:\n{fixed}");
            }
        }
    }

    #[test]
    fn a_query_split_into_literals_joined_with_plus_is_neither_checked_nor_fixed() {
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT  id \" + \"FROM users WHERE id = $1\", 1);\n    let _ = sqlx::query_as!(Row, r#\"\n        select  a\n    \"# + \"from t where x = null\");\n}\n";
        let eng = layout_engine();
        let diags = check_file("f.rs", src, &eng);
        assert!(diags.is_empty(), "{diags:?}");
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert_eq!(out.new_src, None);
        assert!(out.changes.is_empty() && out.layout_kept_lines.is_empty() && out.unparsable_lines.is_empty());
    }

    #[test]
    fn skips_set_statements() {
        assert!(is_skippable("SET audit.skip = 'true'\n"));
        assert!(!is_skippable("SELECT 1\n"));
    }

    #[test]
    fn a_multibyte_char_at_the_start_of_the_sql_is_not_split() {
        assert!(!is_skippable("-- é\nSELECT 1\n"));
        assert!(!is_skippable("é"));
        let src = "fn f() {\n    let _ = sqlx::query!(\"-- é\\nSELECT 1\");\n    let _ = sqlx::query!(r#\"\n        -- é\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n}\n";
        let eng = engine();
        assert!(check_file("f.rs", src, &eng).iter().any(|d| d.code == "CV05" && d.line == 5));
        let fixed = fix_file("f.rs", src, &eng).unwrap().new_src.expect("should change");
        assert!(fixed.contains("x IS NULL"), "{fixed}");
    }

    fn cv05_engine() -> SqruffEngine {
        SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = CV05\n").unwrap()
    }

    #[test]
    fn a_fix_without_sqruff_changes_leaves_string_constants_byte_identical() {
        let src = concat!(
            "fn f() {\n",
            "    let _ = sqlx::query!(r#\"\n",
            "        INSERT INTO t VALUES ('line1\n",
            "          \n",
            "        line3')\n",
            "    \"#);\n",
            "    let _ = sqlx::query!(r#\"\n",
            "\tSELECT 'x\n",
            "    y' AS a\n",
            "    \"#);\n",
            "    let _ = sqlx::query!(\"SELECT 'p\n",
            "          \n",
            "     \t  q' AS b\n",
            "        FROM t\");\n",
            "}\n",
        );
        let out = fix_file("f.rs", src, &cv05_engine()).unwrap();
        assert_eq!(out.new_src, None);
        assert!(out.changes.is_empty() && out.layout_kept_lines.is_empty());
    }

    #[test]
    fn a_fix_keeps_the_bytes_of_untouched_lines_inside_string_constants() {
        let src = concat!(
            "fn f() {\n",
            "    let _ = sqlx::query!(r#\"\n",
            "        SELECT 'line1\n",
            "          \n",
            "            line3' AS a\n",
            "        FROM t WHERE x = NULL\n",
            "    \"#);\n",
            "    let _ = sqlx::query!(r#\"\n",
            "\tSELECT 'x\n",
            "    y' AS b\n",
            "        FROM t WHERE x = NULL\n",
            "    \"#);\n",
            "    let _ = sqlx::query!(\"SELECT 'p\n",
            "          \n",
            "        q' AS c\n",
            "        FROM t WHERE x = NULL\");\n",
            "}\n",
        );
        let fixed = fix_file("f.rs", src, &cv05_engine()).unwrap().new_src.expect("should change");
        assert_eq!(fixed.matches("x IS NULL").count(), 3, "{fixed}");
        for constant in [
            "'line1\n          \n            line3' AS a\n",
            "\tSELECT 'x\n    y' AS b\n",
            "'p\n          \n        q' AS c\n",
        ] {
            assert!(fixed.contains(constant), "{constant:?} changed in:\n{fixed}");
        }
        assert_eq!(fixed.replace("x IS NULL", "x = NULL"), src);
    }

    #[test]
    fn a_fix_keeps_whitespace_only_lines_shorter_equal_or_longer_than_the_indent() {
        let src = concat!(
            "fn f() {\n",
            "    let _ = sqlx::query!(\n",
            "        r#\"\n",
            "        INSERT INTO notes (body) SELECT 'first\n",
            "        \n",
            "   \n",
            "  \n",
            "          \n",
            "\n",
            "        second' FROM t WHERE x = NULL\n",
            "        \"#\n",
            "    );\n",
            "    let _ = sqlx::query!(\"SELECT 'p\n",
            "        \n",
            "   \n",
            "  \n",
            "          \n",
            "        q' AS c\n",
            "        FROM t WHERE x = NULL\");\n",
            "}\n",
        );
        let fixed = fix_file("f.rs", src, &cv05_engine()).unwrap().new_src.expect("should change");
        assert_eq!(fixed.matches("x IS NULL").count(), 2, "{fixed}");
        assert_eq!(fixed.replace("x IS NULL", "x = NULL"), src);
    }

    #[test]
    fn a_one_liner_whose_fix_would_fold_a_comment_is_reported_not_applied() {
        let eng =
            SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = LT09,CV05\n").unwrap();
        let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a, b FROM t WHERE x = NULL -- note\");\n    let _ = sqlx::query!(\"SELECT a, b FROM t WHERE x = NULL\");\n}\n";
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert_eq!(out.layout_kept_lines, [2]);
        let fixed = out.new_src.expect("the second literal is fixed");
        assert!(fixed.contains("x = NULL -- note"), "{fixed}");
        assert!(fixed.contains("\"SELECT a, b FROM t WHERE x IS NULL\""), "{fixed}");
    }

    #[test]
    fn a_one_liner_whose_fix_only_adds_line_breaks_is_reported_not_applied() {
        let eng = SqruffEngine::from_source("[sqruff]\ndialect = postgres\n").unwrap();
        let src = "fn f() {\n    let _ = sqlx::query!(\"with x as (select 1 as a) select a from x\");\n}\n";
        assert!(!check_file("f.rs", src, &eng).is_empty());
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert_eq!(out.new_src, None);
        assert_eq!(out.layout_kept_lines, [2]);
    }

    #[test]
    fn sql_that_does_not_fully_parse_is_not_rewritten() {
        // `REGEXP` is not postgres: sqruff leaves it unparsable and its AL02 fix
        // would turn the operator into an alias, changing what the query does.
        let eng = SqruffEngine::from_source("[sqruff]\ndialect = postgres\nrules = AL02\n").unwrap();
        let src = "fn f() {\n    let _ = sqlx::query(\"SELECT 123 REGEXP '23'\");\n}\n";
        let out = fix_file("f.rs", src, &eng).unwrap();
        assert_eq!(out.new_src, None);
        assert_eq!(out.unparsable_lines, [2]);
    }

    #[test]
    fn a_literal_on_the_first_line_after_a_byte_order_mark_is_checked_and_fixed() {
        let src = "\u{feff}fn f(){let _=sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");}\nfn g(){let _=sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");}\n";
        assert_eq!(positions(src), [(1, 52, "CV05".to_string()), (2, 52, "CV05".to_string())]);
        let fixed = fix_file("f.rs", src, &engine()).unwrap().new_src.expect("should change");
        assert_eq!(fixed, src.replace("x = NULL", "x IS NULL"));
    }
}
