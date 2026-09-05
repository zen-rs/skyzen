//! Keep local secret files out of git.
//!
//! `.env` and `.env.local` hold credentials. Scaffolded `.gitignore` lists them; this module
//! fails the CLI load when git is already tracking one (a 100% leak) and warns when a path is not
//! ignored (`git add .` would stage it).
//!
//! Not `.dev.vars`: it is generated into `.skyzen/gen/`, which is ignored whole, and nothing puts
//! one in the project root any more.

use anyhow::Result;
use std::{
    path::Path,
    process::{Command, Stdio},
};

/// Files that must not be committed.
pub const SECRET_FILES: &[&str] = &[".env", ".env.local"];

/// Inspect `root` for secret files git would commit.
///
/// Returns heuristic warnings (path exists in the work tree and is not ignored). A tracked
/// file is an error. Not a git repository, or `git` missing: skip.
///
/// # Errors
///
/// Fails when any of [`SECRET_FILES`] is tracked by git.
pub fn ensure(root: &Path) -> Result<Vec<String>> {
    if !is_git_work_tree(root) {
        return Ok(Vec::new());
    }

    let mut warnings = Vec::new();
    let mut tracked = Vec::new();
    for file in SECRET_FILES {
        if is_tracked(root, file) {
            tracked.push(*file);
            continue;
        }
        if !is_ignored(root, file) {
            warnings.push(format!(
                "`{file}` is not gitignored; `git add .` would stage it. Add the name to .gitignore."
            ));
        }
    }

    if !tracked.is_empty() {
        let names = tracked
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "{names} tracked by git. Remove from the index (`git rm --cached -- {names}`) and \
             keep the name in .gitignore so the secret never reaches the remote."
        );
    }

    Ok(warnings)
}

fn is_git_work_tree(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
    })
}

fn is_tracked(root: &Path, file: &str) -> bool {
    git(root, &["ls-files", "--error-unmatch", "--", file])
        .is_some_and(|output| output.status.success())
}

fn is_ignored(root: &Path, file: &str) -> bool {
    git(root, &["check-ignore", "-q", "--", file]).is_some_and(|output| output.status.success())
}

fn git(root: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::ensure;
    use std::{fs, process::Command};

    fn git_init(root: &std::path::Path) {
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .expect("git init");
        assert!(status.success(), "git init");
        // check-ignore and ls-files need identity only when committing; adding does not.
    }

    #[test]
    fn a_non_git_directory_is_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(".env"), "TOKEN=aaaa\n").expect("write");
        let warnings = ensure(dir.path()).expect("not a git repo");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_gitignored_env_file_is_ok() {
        let dir = tempfile::tempdir().expect("temp dir");
        git_init(dir.path());
        fs::write(dir.path().join(".gitignore"), ".env\n.env.local\n").expect("gitignore");
        fs::write(dir.path().join(".env"), "TOKEN=aaaa\n").expect("write");
        let warnings = ensure(dir.path()).expect("ignored");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_tracked_env_file_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        git_init(dir.path());
        fs::write(dir.path().join(".env"), "TOKEN=aaaa\n").expect("write");
        let add = Command::new("git")
            .args(["add", "-f", "--", ".env"])
            .current_dir(dir.path())
            .status()
            .expect("git add");
        assert!(add.success(), "git add");

        let error = ensure(dir.path()).expect_err("tracked");
        let rendered = error.to_string();
        assert!(rendered.contains(".env"), "{rendered}");
        assert!(rendered.contains("tracked"), "{rendered}");
        assert!(!rendered.contains("aaaa"), "{rendered}");
    }

    #[test]
    fn an_unignored_env_path_is_a_warning() {
        let dir = tempfile::tempdir().expect("temp dir");
        git_init(dir.path());
        let warnings = ensure(dir.path()).expect("unignored is not a block");
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(".env") && line.contains("not gitignored")),
            "{warnings:?}"
        );
    }
}
