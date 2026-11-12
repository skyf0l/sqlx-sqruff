//! `--config` is applied on top of the discovered sqruff config, as in sqruff's
//! own CLI, instead of replacing it.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// `SELECT  a` has a double space, which LT01 reports.
const LT01_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT  a FROM t\");\n}\n";
/// MySQL backtick identifiers do not parse as postgres.
const MYSQL_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT `a` FROM t\");\n}\n";

/// A throwaway project directory with a `.sqruff` and an extra config file,
/// removed on drop. It also serves as `HOME` and `XDG_CONFIG_HOME`, so no
/// user-level sqruff config can affect the result.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str, project_config: &str, extra_config: &str, src: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sqlx_sqruff_stack_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("q.rs"), src).unwrap();
        fs::write(dir.join(".sqruff"), project_config).unwrap();
        fs::write(dir.join("extra.cfg"), extra_config).unwrap();
        Self(dir)
    }

    fn check(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
            .args(["check", "q.rs", "--config", "extra.cfg"])
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .output()
            .unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn report(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_project_rules_stay_when_the_extra_config_does_not_set_rules() {
    let p = Project::new(
        "keeps_rules",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\nmax_line_length = 100\n",
        LT01_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn the_extra_config_overrides_the_project_dialect() {
    let p = Project::new(
        "dialect",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\ndialect = mysql\n",
        MYSQL_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

/// Run the binary in `dir`, which also serves as `HOME`, `XDG_CONFIG_HOME` and
/// `LOCALAPPDATA`. With the current directory equal to `HOME`, sqruff reads
/// config files only from the user config directory, `HOME` and the current
/// directory, so no config file in a parent of `dir` can be read.
fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("LOCALAPPDATA", dir)
        .output()
        .unwrap()
}

fn notes(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr).lines().filter(|l| l.starts_with("note:")).map(str::to_string).collect()
}

#[test]
fn a_note_says_when_no_config_file_sets_a_sqruff_option() {
    let dir = std::env::temp_dir().join(format!("sqlx_sqruff_stack_{}_no_config", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("q.rs"), LT01_QUERY).unwrap();
    let runs: [&[&str]; 3] = [&["check", "q.rs"], &["check", "q.rs", "--format", "jsonl"], &["fix", "--check", "q.rs"]];

    // No config file, then a `.sqruff` that holds only this tool's section,
    // which sqruff does not read.
    for sqruff in [None, Some("[sqlx-sqruff]\nexclude = gen/**\n")] {
        if let Some(config) = sqruff {
            fs::write(dir.join(".sqruff"), config).unwrap();
        }
        for args in runs {
            let out = run_in(&dir, args);
            assert_ne!(out.status.code(), Some(2), "{sqruff:?} {args:?}: {}", report(&out));
            let notes = notes(&out);
            assert_eq!(notes.len(), 1, "{sqruff:?} {args:?}: {}", report(&out));
            assert!(notes[0].contains("default"), "{sqruff:?} {args:?}: {}", report(&out));
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(!stdout.contains("note:"), "{sqruff:?} {args:?}: {}", report(&out));
            if args.contains(&"jsonl") {
                assert!(!stdout.is_empty(), "{sqruff:?} {args:?}: {}", report(&out));
                for line in stdout.lines() {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
                    assert!(value.is_object(), "{line:?}");
                }
            }
        }
    }

    fs::write(dir.join(".sqruff"), "[sqruff]\ndialect = postgres\n").unwrap();
    let out = run_in(&dir, &["check", "q.rs"]);
    assert!(notes(&out).is_empty(), "{}", report(&out));

    fs::remove_file(dir.join(".sqruff")).unwrap();
    fs::write(dir.join("extra.cfg"), "[sqruff]\nrules = CV05\n").unwrap();
    let out = run_in(&dir, &["check", "q.rs", "--config", "extra.cfg"]);
    assert!(notes(&out).is_empty(), "{}", report(&out));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_dialect_in_the_extra_config_is_an_error_naming_it() {
    let p = Project::new(
        "bad_dialect",
        "[sqruff]\ndialect = postgres\nrules = CV05\n",
        "[sqruff]\ndialect = mysqll\n",
        MYSQL_QUERY,
    );
    let out = p.check();
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("mysqll"), "{}", report(&out));
}
