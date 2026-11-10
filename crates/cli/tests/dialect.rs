//! Dialect resolution: `--dialect`, then the config's `dialect`, then postgres.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

/// MySQL backtick identifiers do not parse as postgres.
const MYSQL_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT `a` FROM t\");\n}\n";
/// A `::` cast parses as postgres but not as ansi, sqruff's own default.
const POSTGRES_QUERY: &str = "fn f() {\n    let _ = sqlx::query(\"SELECT a::int FROM t\");\n}\n";

/// A throwaway project directory, removed on drop. It also serves as `HOME` and
/// `XDG_CONFIG_HOME`, so no user-level sqruff config can affect the result.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str, sqruff: Option<&str>, src: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("sqlx_sqruff_dialect_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("q.rs"), src).unwrap();
        if let Some(config) = sqruff {
            fs::write(dir.join(".sqruff"), config).unwrap();
        }
        Self(dir)
    }

    fn check(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
            .arg("check")
            .arg("q.rs")
            .args(args)
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
fn the_dialect_set_in_the_project_config_is_used() {
    let p = Project::new("from_config", Some("[sqruff]\ndialect = mysql\nrules = CV05\n"), MYSQL_QUERY);
    let out = p.check(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn the_dialect_flag_overrides_the_project_config() {
    let p = Project::new("flag_wins", Some("[sqruff]\ndialect = mysql\nrules = CV05\n"), MYSQL_QUERY);
    let out = p.check(&["--dialect", "postgres"]);
    assert_eq!(out.status.code(), Some(1), "{}", report(&out));
}

#[test]
fn postgres_is_used_when_no_config_sets_a_dialect() {
    let p = Project::new("default", Some("[sqruff]\nrules = CV05\n"), POSTGRES_QUERY);
    let out = p.check(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn the_dialect_flag_accepts_every_sqruff_dialect() {
    let p = Project::new("flag_mariadb", Some("[sqruff]\nrules = CV05\n"), MYSQL_QUERY);
    let out = p.check(&["--dialect", "MariaDB"]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
}

#[test]
fn an_unknown_dialect_flag_is_an_error_naming_it() {
    let p = Project::new("flag_typo", Some("[sqruff]\nrules = CV05\n"), MYSQL_QUERY);
    let out = p.check(&["--dialect", "mysqll"]);
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("mysqll"), "{}", report(&out));
}

#[test]
fn an_unknown_dialect_in_the_config_is_an_error_not_a_crash() {
    let p = Project::new("typo", Some("[sqruff]\ndialect = mysqll\n"), MYSQL_QUERY);
    let out = p.check(&[]);
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("mysqll"), "{}", report(&out));
}
