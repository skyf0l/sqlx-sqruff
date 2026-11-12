//! The one spelling of a path in the output, whichever way it was named.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf, Prefix},
    sync::LazyLock,
};

/// The current directory as the OS reports it, and resolved, both without a
/// verbatim prefix.
static CURRENT_DIR: LazyLock<Option<(PathBuf, PathBuf)>> = LazyLock::new(|| {
    let dir = std::env::current_dir().ok()?;
    let canonical = dir.canonicalize().ok()?;
    Some((without_verbatim_prefix(&dir), without_verbatim_prefix(&canonical)))
});

/// `path` relative to the current directory when it is inside it, without a
/// leading `./`; otherwise absolute. A path that exists and is outside the
/// current directory is resolved, so `../src/a.rs` and `/abs/src/a.rs` print
/// the same.
pub fn shown(path: &Path) -> PathBuf {
    let Some((dir, canonical_dir)) = CURRENT_DIR.as_ref() else {
        return path.to_path_buf();
    };
    let absolute = std::path::absolute(path).ok().map(|absolute| without_verbatim_prefix(&absolute));
    if let Some(absolute) = &absolute
        && let Some(relative) = inside(absolute, dir).or_else(|| inside(absolute, canonical_dir))
    {
        return relative;
    }
    match path.canonicalize() {
        Ok(canonical) => {
            let canonical = without_verbatim_prefix(&canonical);
            inside(&canonical, canonical_dir).unwrap_or(canonical)
        }
        Err(_) => absolute.unwrap_or_else(|| path.to_path_buf()),
    }
}

/// `path` relative to `dir`, or `None` when it is not inside `dir` without
/// going through a `..`.
fn inside(path: &Path, dir: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.strip_prefix(dir).ok()?.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Some(relative)
}

/// `path` without the `\\?\` prefix that `canonicalize` adds on Windows, which
/// users do not write and many tools do not accept.
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path.to_path_buf();
    };
    let mut plain = OsString::new();
    match prefix.kind() {
        Prefix::VerbatimDisk(letter) => plain.push(format!("{}:", char::from(letter))),
        Prefix::VerbatimUNC(server, share) => {
            plain.push(r"\\");
            plain.push(server);
            plain.push(r"\");
            plain.push(share);
        }
        _ => return path.to_path_buf(),
    }
    let mut plain = PathBuf::from(plain);
    plain.extend(components);
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_inside_the_directory_is_relative_without_a_leading_dot() {
        let dir = Path::new("/project");
        let expected = ["src", "a.rs"].iter().collect::<PathBuf>();
        assert_eq!(inside(Path::new("/project/src/a.rs"), dir), Some(expected.clone()));
        assert_eq!(inside(Path::new("/project/./src/a.rs"), dir), Some(expected));
        assert_eq!(inside(Path::new("/project"), dir), Some(PathBuf::from(".")));
    }

    #[test]
    fn a_path_outside_the_directory_or_through_a_parent_is_not_inside() {
        let dir = Path::new("/project");
        assert_eq!(inside(Path::new("/other/a.rs"), dir), None);
        assert_eq!(inside(Path::new("/project/../project/a.rs"), dir), None);
        assert_eq!(inside(Path::new("/projects/a.rs"), dir), None);
    }

    #[test]
    fn a_path_without_a_verbatim_prefix_is_unchanged() {
        let path = Path::new("/project/src/a.rs");
        assert_eq!(without_verbatim_prefix(path), path);
    }

    #[cfg(windows)]
    #[test]
    fn the_verbatim_prefix_of_a_canonical_windows_path_is_removed() {
        assert_eq!(without_verbatim_prefix(Path::new(r"\\?\C:\project\a.rs")), Path::new(r"C:\project\a.rs"));
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\server\share\a.rs")),
            Path::new(r"\\server\share\a.rs")
        );
    }
}
