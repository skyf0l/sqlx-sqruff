//! Exit status, warnings and output of the CLI, through the real binary.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

const CV05: &str = "[sqruff]\ndialect = postgres\nrules = CV05\n";
/// The `.rs` file of a [`Project`], as the tool prints it when run from the
/// project directory.
const FILE: &str = "q.rs";

/// A throwaway directory holding one sqruff config and one `.rs` file, removed
/// on drop. It also serves as `HOME` and `XDG_CONFIG_HOME`, so no user-level
/// sqruff config can affect the result.
struct Project {
    dir: PathBuf,
    file: PathBuf,
    config: PathBuf,
}

impl Project {
    fn new(tag: &str, config: &str, src: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sqlx_sqruff_cli_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE);
        let config_path = dir.join("test.sqruff");
        fs::write(&file, src).unwrap();
        fs::write(&config_path, config).unwrap();
        Self { dir, file, config: config_path }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"));
        command.current_dir(&self.dir).env("HOME", &self.dir).env("XDG_CONFIG_HOME", &self.dir);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .arg(&self.file)
            .arg("--config")
            .arg(&self.config)
            .output()
            .unwrap()
    }

    fn src(&self) -> String {
        fs::read_to_string(&self.file).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stderr lines that name the project's file at `line`.
fn lines_naming(out: &Output, line: usize) -> Vec<String> {
    let at = format!(" {FILE}:{line}:");
    stderr(out).lines().filter(|l| l.contains(&at)).map(str::to_string).collect()
}

#[test]
fn fix_check_passes_a_clean_file() {
    let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a FROM t WHERE x IS NULL\");\n}\n";
    let p = Project::new("clean", CV05, src);
    let out = p.run(&["fix", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn fix_check_fails_a_fixable_file_without_writing_it() {
    let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");\n}\n";
    let p = Project::new("fixable", CV05, src);
    let out = p.run(&["fix", "--check"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(p.src(), src);
}

#[test]
fn fix_check_shows_each_query_before_and_after() {
    let src = "fn f() {\n    let _ = sqlx::query!(r#\"\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n}\n";
    let p = Project::new("show_change", CV05, src);
    let out = p.run(&["fix", "--check"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    let at = format!("{FILE}:2");
    let start = lines.iter().position(|l| *l == at).unwrap_or_else(|| panic!("no {at} in:\n{stdout}"));
    // The literal spans 3 source lines: 3 removed lines, then 3 added lines.
    let change = &lines[start + 1..start + 7];
    assert!(change[..3].iter().all(|l| l.starts_with("- ")), "{stdout}");
    assert!(change[3..].iter().all(|l| l.starts_with("+ ")), "{stdout}");
    assert!(change[..3].iter().any(|l| l.contains("x = NULL")), "{stdout}");
    assert!(change[3..].iter().any(|l| l.contains("x IS NULL")), "{stdout}");
}

#[test]
fn list_and_dump_report_a_file_that_does_not_parse_as_an_error() {
    let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT 1\");\n";
    let p = Project::new("unparsable", CV05, src);
    let file = format!("error: {FILE}: ");
    for command in ["list", "dump"] {
        let out = p.run(&[command]);
        assert_eq!(out.status.code(), Some(2), "{command}: {}", stderr(&out));
        assert!(stderr(&out).lines().any(|l| l.starts_with(&file)), "{command}: {}", stderr(&out));
    }
}

#[test]
fn a_query_split_into_literals_joined_with_plus_is_skipped_by_every_command() {
    let src = "fn f() {\n    let _ = sqlx::query!(\"SELECT  id \" + \"FROM users WHERE id = $1\", 1);\n}\n";
    let p = Project::new("plus", "[sqruff]\ndialect = postgres\nrules = LT01,CV05\n", src);
    let at = format!("{FILE}:2");

    let check = p.run(&["check"]);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
    assert!(!String::from_utf8_lossy(&check.stdout).contains(&at), "{}", String::from_utf8_lossy(&check.stdout));

    for args in [&["fix", "--check"][..], &["fix"]] {
        let out = p.run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert!(lines_naming(&out, 2).is_empty(), "{args:?}: {}", stderr(&out));
        assert_eq!(p.src(), src, "{args:?}");
    }

    for command in ["list", "dump"] {
        let out = p.run(&[command]);
        assert_eq!(out.status.code(), Some(0), "{command}: {}", stderr(&out));
        assert!(!String::from_utf8_lossy(&out.stdout).contains(&at), "{command}: {}", String::from_utf8_lossy(&out.stdout));
    }
}

#[test]
fn a_dropped_edit_fails_fix_check_and_is_reported_by_fix() {
    // Quoting identifiers inside `r"..."` would end the raw string early.
    let config = "[sqruff]\ndialect = postgres\nrules = RF06\n\n\
                  [sqruff:rules:references.quoting]\nprefer_quoted_identifiers = True\n";
    let src = "fn f() {\n    let _ = sqlx::query!(r\"SELECT a FROM t\");\n}\n";
    let p = Project::new("dropped", config, src);

    let check = p.run(&["fix", "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    assert!(!lines_naming(&check, 2).is_empty(), "{}", stderr(&check));

    let fix = p.run(&["fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    let warnings = lines_naming(&fix, 2);
    assert!(warnings.iter().any(|l| l.contains("not applied")), "{}", stderr(&fix));
    assert_eq!(p.src(), src);
}

#[test]
fn a_literal_with_other_escapes_needing_a_fix_fails_fix_check() {
    let src = "fn f() {\n    let _ = sqlx::query(\"SELECT a\\nFROM t WHERE x = NULL\");\n}\n";
    let p = Project::new("escapes", CV05, src);

    let check = p.run(&["fix", "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    let file = format!(" {FILE}: ");
    let notes: Vec<String> = stderr(&check)
        .lines()
        .filter(|l| l.contains(&file) && l.contains("raw string"))
        .map(str::to_string)
        .collect();
    assert_eq!(notes.len(), 1, "{}", stderr(&check));
    assert_eq!(p.src(), src);

    let fix = p.run(&["fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    assert_eq!(p.src(), src);
}

#[test]
fn sql_that_does_not_parse_is_not_rewritten_and_fails_fix_check() {
    let src = "fn f() {\n    let _ = sqlx::query(\"SELECT 123 REGEXP '23'\");\n}\n";
    let p = Project::new("unparsable_sql", "[sqruff]\ndialect = postgres\nrules = AL02\n", src);

    let check = p.run(&["fix", "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    let warnings = lines_naming(&check, 2);
    assert!(warnings.iter().any(|l| l.starts_with("warning:") && l.contains("not applied")), "{}", stderr(&check));

    let fix = p.run(&["fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    assert_eq!(p.src(), src);
}

#[test]
fn a_fix_that_needs_a_different_line_layout_fails_fix_check_and_is_reported() {
    // sqruff's fix puts a blank line after the CTE, which a one-line literal
    // cannot hold.
    let src = "fn f() {\n    let _ = sqlx::query!(\"with x as (select 1 as a) select a from x\");\n}\n";
    let p = Project::new("layout", "[sqruff]\ndialect = postgres\n", src);

    let check = p.run(&["fix", "--check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    let warnings = lines_naming(&check, 2);
    assert!(warnings.iter().any(|l| l.starts_with("warning:") && l.contains("not applied")), "{}", stderr(&check));

    let fix = p.run(&["fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    assert!(!lines_naming(&fix, 2).is_empty(), "{}", stderr(&fix));
    assert_eq!(p.src(), src);
}

/// The `N` of the `N not applied` count in the summary that `fix` prints.
fn not_applied_count(out: &Output) -> usize {
    let err = stderr(out);
    let summary = err
        .lines()
        .find(|l| l.contains(" not applied;") && l.contains(" file(s) failed."))
        .unwrap_or_else(|| panic!("no summary in:\n{err}"));
    let before = summary.split(" not applied;").next().unwrap_or_default();
    before.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no count in {summary:?}"))
}

#[test]
fn a_fix_whose_line_breaks_cannot_be_applied_is_counted_as_not_applied_by_fix_and_fix_check() {
    // sqruff removes the double space and breaks the too-long line; a one-line
    // literal can take the first change but not the second.
    let src = "fn f() {\n    let _ = sqlx::query!(\"INSERT INTO flow (workspace_id, path, summary, description, value, edited_by, schema, tag) VALUES  ($1, $2, $3, $4, $5, $6, $7, $8)\");\n}\n";
    let p = Project::new("partly_applied", "[sqruff]\ndialect = postgres\nrules = LT01,LT05\n", src);

    let fix = p.run(&["fix"]);
    assert_eq!(fix.status.code(), Some(0), "{}", stderr(&fix));
    assert!(p.src().contains("VALUES ($1"), "{}", p.src());
    assert_eq!(not_applied_count(&fix), 1, "{}", stderr(&fix));
    let warnings = lines_naming(&fix, 2);
    assert!(warnings.iter().any(|l| l.contains("not applied") && l.contains("LT05")), "{}", stderr(&fix));
    let fixed = p.src();

    let fix_check = p.run(&["fix", "--check"]);
    assert_eq!(fix_check.status.code(), Some(1), "{}", stderr(&fix_check));
    assert!(!String::from_utf8_lossy(&fix_check.stdout).contains("would fix"), "{}", String::from_utf8_lossy(&fix_check.stdout));
    assert_eq!(not_applied_count(&fix_check), not_applied_count(&fix), "{}", stderr(&fix_check));
    let warnings = lines_naming(&fix_check, 2);
    assert!(warnings.iter().any(|l| l.contains("not applied") && l.contains("LT05")), "{}", stderr(&fix_check));

    let check = p.run(&["check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
    let at = format!("{FILE}:2:");
    let findings = String::from_utf8_lossy(&check.stdout).into_owned();
    assert!(findings.lines().all(|l| l.starts_with(&at) && l.contains("LT05")), "{findings}");

    let again = p.run(&["fix"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(not_applied_count(&again), not_applied_count(&fix), "{}", stderr(&again));
    assert_eq!(p.src(), fixed);
}

#[test]
fn an_invalid_config_value_is_an_error_not_a_crash() {
    let p = Project::new("bad_value", "[sqruff]\ndialect = postgres\nmax_line_length = abc\n", "fn f() {}\n");
    let out = p.run(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("invalid sqruff config"), "{}", stderr(&out));
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}

#[test]
fn a_missing_config_file_is_an_error_not_a_crash() {
    let p = Project::new("missing_config", CV05, "fn f() {}\n");
    let missing = p.dir.join("missing.sqruff");
    let out = p.command().arg("check").arg(&p.file).arg("--config").arg(&missing).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains(&missing.display().to_string()), "{}", stderr(&out));
}

#[test]
fn a_query_sqruff_cannot_process_fails_check_and_fix_check() {
    let src = "fn f() {\n    let _ = sqlx::query!(r#\"\n        -- sqruff:dialect:not_a_dialect\n        SELECT a FROM t WHERE x = NULL\n    \"#);\n}\n";
    let p = Project::new("sqruff_error", CV05, src);

    let check = p.run(&["check"]);
    assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));

    let fix_check = p.run(&["fix", "--check"]);
    assert_eq!(fix_check.status.code(), Some(1), "{}", stderr(&fix_check));
    assert!(!lines_naming(&fix_check, 2).is_empty(), "{}", stderr(&fix_check));
    assert_eq!(p.src(), src);
}
