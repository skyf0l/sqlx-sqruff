//! CLI-only settings read from a `[sqlx-sqruff]` section of the project
//! `.sqruff`.
//!
//! sqruff's own config parser DROPS every section it doesn't recognise (only
//! `sqruff` / `sqruff:*` and the `sqlfluff` aliases survive), so a
//! `[sqlx-sqruff]` section is invisible to the linter — it is ours alone. We
//! read it with a tiny INI scan rather than through `FluffConfig`, whose section
//! accessors (`get` / `get_section`) panic on an absent section.
//!
//! Defaults reproduce "scan every `.rs`, respecting `.gitignore`", so the
//! section is purely opt-in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const SECTION: &str = "sqlx-sqruff";
const CONFIG_FILE: &str = ".sqruff";

/// File-selection settings. See the module docs for the include/exclude matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliSettings {
    /// Whitelist globs. When set (and no explicit paths are passed), only
    /// matching files are scanned.
    pub include: Vec<String>,
    /// Blacklist globs, always applied on top of `include` / explicit paths.
    pub exclude: Vec<String>,
    /// Honour `.gitignore` / `.ignore` / `.git/info/exclude` during the walk.
    pub respect_gitignore: bool,
}

impl Default for CliSettings {
    fn default() -> Self {
        Self { include: Vec::new(), exclude: Vec::new(), respect_gitignore: true }
    }
}

impl CliSettings {
    /// Load from `extra_config` (an explicit `--config`) when given, otherwise
    /// the nearest `.sqruff` found walking up from the cwd. A missing file or a
    /// missing `[sqlx-sqruff]` section yields [`CliSettings::default`].
    pub fn load(extra_config: Option<&Path>) -> Result<Self> {
        let path = match extra_config {
            Some(p) => Some(p.to_path_buf()),
            None => find_config()?,
        };
        let Some(path) = path else { return Ok(Self::default()) };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    /// Parse the `[sqlx-sqruff]` section out of `.sqruff` text. Unknown keys are
    /// ignored; absent keys keep their default.
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        let mut in_section = false;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_section = header.trim() == SECTION;
                continue;
            }
            if !in_section {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            match key.trim() {
                "include" => settings.include = split_globs(value),
                "exclude" => settings.exclude = split_globs(value),
                "respect_gitignore" => settings.respect_gitignore = parse_bool(value.trim(), true),
                _ => {}
            }
        }
        settings
    }
}

/// Split a comma-separated glob list, trimming each entry and dropping blanks.
fn split_globs(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned).collect()
}

/// Parse a permissive boolean, falling back to `default` on anything unexpected.
fn parse_bool(value: &str, default: bool) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => default,
    }
}

/// Walk up from the cwd to the filesystem root, returning the first `.sqruff`.
fn find_config() -> Result<Option<PathBuf>> {
    let mut dir = std::env::current_dir().context("resolving current directory")?;
    loop {
        let candidate = dir.join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        if !dir.pop() {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_section_absent() {
        let s = CliSettings::parse("[sqruff]\ndialect = postgres\n");
        assert_eq!(s, CliSettings::default());
        assert!(s.respect_gitignore);
    }

    #[test]
    fn reads_include_exclude_and_trims() {
        let s = CliSettings::parse(
            "[sqlx-sqruff]\ninclude = tests/**/*.rs, crates/**/*.rs\nexclude = **/target/**\n",
        );
        assert_eq!(s.include, ["tests/**/*.rs", "crates/**/*.rs"]);
        assert_eq!(s.exclude, ["**/target/**"]);
        assert!(s.respect_gitignore);
    }

    #[test]
    fn respect_gitignore_false_and_section_isolation() {
        // Keys outside [sqlx-sqruff] must not leak in; `respect_gitignore` parses.
        let s = CliSettings::parse(
            "[sqruff]\ninclude = should_be_ignored\n[sqlx-sqruff]\nrespect_gitignore = false\n",
        );
        assert!(s.include.is_empty());
        assert!(!s.respect_gitignore);
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let s = CliSettings::parse(
            "[sqlx-sqruff]\n# a comment\n\ninclude = a.rs\n; semicolon comment\nexclude = b.rs\n",
        );
        assert_eq!(s.include, ["a.rs"]);
        assert_eq!(s.exclude, ["b.rs"]);
    }
}
