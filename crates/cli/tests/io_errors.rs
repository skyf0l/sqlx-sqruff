//! A file that cannot be read or written is reported by name, and the run goes
//! on with the next file, through the real binary.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

const CONFIG: &str = "[sqruff]\ndialect = postgres\nrules = CV05\n";
const FIXABLE: &str = "fn f() {\n    let _ = sqlx::query!(\"SELECT a FROM t WHERE x = NULL\");\n}\n";

/// A throwaway project directory with a `.sqruff`, removed on drop. It also
/// serves as `HOME` and `XDG_CONFIG_HOME`, so no user-level sqruff config can
/// affect the result.
struct Project(PathBuf);

impl Project {
    fn new(tag: &str, files: &[(&str, &[u8])]) -> Self {
        let dir = std::env::temp_dir().join(format!("sqlx_sqruff_io_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".sqruff"), CONFIG).unwrap();
        for (rel, contents) in files {
            fs::write(dir.join(rel), contents).unwrap();
        }
        Self(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sqlx-sqruff"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .output()
            .unwrap()
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.path(rel)).unwrap()
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

fn stderr_has(out: &Output, prefix: &str, part: &str) -> bool {
    String::from_utf8_lossy(&out.stderr).lines().any(|l| l.starts_with(prefix) && l.contains(part))
}

#[test]
fn a_file_that_is_not_utf8_is_named_and_the_run_continues() {
    let p = Project::new(
        "not_utf8",
        &[("a.rs", FIXABLE.as_bytes()), ("b.rs", b"fn f() {}\n\xff\xfe\n"), ("c.rs", FIXABLE.as_bytes())],
    );
    for command in [&["check"][..], &["fix", "--check"], &["list"], &["dump"]] {
        let mut args = command.to_vec();
        args.extend(["a.rs", "b.rs", "c.rs"]);
        let out = p.run(&args);
        assert_eq!(out.status.code(), Some(2), "{command:?}: {}", report(&out));
        assert!(stderr_has(&out, "error: b.rs:", "UTF-8"), "{command:?}: {}", report(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        for file in ["a.rs:", "c.rs:"] {
            assert!(stdout.contains(file), "{command:?}: {}", report(&out));
        }
    }
    let check = p.run(&["check", "a.rs", "b.rs", "c.rs"]);
    assert!(stderr_has(&check, "", "finding(s)"), "{}", report(&check));
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_written_is_named_and_fix_continues() {
    let p = Project::new(
        "read_only",
        &[("a.rs", FIXABLE.as_bytes()), ("b.rs", FIXABLE.as_bytes()), ("c.rs", FIXABLE.as_bytes())],
    );
    let mut read_only = fs::metadata(p.path("b.rs")).unwrap().permissions();
    read_only.set_readonly(true);
    fs::set_permissions(p.path("b.rs"), read_only).unwrap();

    let out = p.run(&["fix", "a.rs", "b.rs", "c.rs"]);
    assert_eq!(out.status.code(), Some(2), "{}", report(&out));
    assert!(stderr_has(&out, "error: b.rs:", ""), "{}", report(&out));
    assert!(stderr_has(&out, "", "file(s) failed"), "{}", report(&out));
    assert_eq!(p.read("b.rs"), FIXABLE.as_bytes());
    for fixed in ["a.rs", "c.rs"] {
        assert_ne!(p.read(fixed), FIXABLE.as_bytes(), "{fixed}: {}", report(&out));
    }
}

#[cfg(unix)]
#[test]
fn fix_keeps_the_file_mode_and_leaves_no_temporary_file() {
    use std::os::unix::fs::PermissionsExt;

    let p = Project::new("mode", &[("a.rs", FIXABLE.as_bytes())]);
    fs::set_permissions(p.path("a.rs"), fs::Permissions::from_mode(0o600)).unwrap();

    let out = p.run(&["fix", "a.rs"]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
    assert_ne!(p.read("a.rs"), FIXABLE.as_bytes(), "{}", report(&out));
    let mode = fs::metadata(p.path("a.rs")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let mut entries: Vec<String> = fs::read_dir(&p.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, [".sqruff", "a.rs"]);
}

#[cfg(unix)]
#[test]
fn fix_writes_through_a_symlink_named_on_the_command_line() {
    let p = Project::new("symlink", &[]);
    fs::create_dir_all(p.path("real")).unwrap();
    fs::write(p.path("real/q.rs"), FIXABLE).unwrap();
    std::os::unix::fs::symlink("real/q.rs", p.path("link.rs")).unwrap();

    let out = p.run(&["fix", "link.rs"]);
    assert_eq!(out.status.code(), Some(0), "{}", report(&out));
    assert!(fs::symlink_metadata(p.path("link.rs")).unwrap().file_type().is_symlink());
    assert_ne!(p.read("real/q.rs"), FIXABLE.as_bytes(), "{}", report(&out));
}
