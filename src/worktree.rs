//! Worktrees: temporary directories that hold a materialized tree plus symlinks to the
//! checkout's dependency dirs. They are not registered with `git worktree`, so the user's
//! worktree list never changes, even if titer is killed.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::RunnerConfig;
use crate::git::Git;

#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
}

impl Worktree {
    /// Materializes `tree` at `dest` and links each runner's dependency dirs from the checkout.
    pub fn create(
        git: &Git,
        tree: &str,
        dest: &Path,
        runners: &[RunnerConfig],
    ) -> anyhow::Result<Worktree> {
        let scratch_index = dest.with_extension("index");
        git.materialize(tree, dest, &scratch_index)
            .with_context(|| format!("cannot materialize {tree} at {}", dest.display()))?;
        link_deps(git.root(), dest, runners)?;
        Ok(Worktree {
            path: dest.to_path_buf(),
        })
    }

    /// Creates `count` worktrees of `tree` in parallel under `parent`, named `{prefix}{i}`.
    pub fn create_many(
        git: &Git,
        tree: &str,
        parent: &Path,
        prefix: &str,
        count: usize,
        runners: &[RunnerConfig],
    ) -> anyhow::Result<Vec<Worktree>> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..count)
                .map(|i| {
                    let dest = parent.join(format!("{prefix}{i}"));
                    scope.spawn(move || Worktree::create(git, tree, &dest, runners))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("worktree thread panicked"))
                .collect()
        })
    }

    /// Writes `contents` to the repo-relative `path`, replacing what is there. Refuses a path
    /// under a symlinked dir, such as a dependency dir linked from the user's checkout.
    pub fn write(&self, path: &str, contents: &[u8]) -> anyhow::Result<()> {
        self.check_inside(path)?;
        let target = self.path.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Never write through a symlink into the user's checkout.
        if target.is_symlink() {
            std::fs::remove_file(&target)?;
        }
        std::fs::write(&target, contents)
            .with_context(|| format!("cannot write {}", target.display()))
    }

    /// Puts back a file's original contents, even if a test made it read-only or replaced it.
    pub fn restore(&self, path: &str, contents: &[u8]) -> anyhow::Result<()> {
        self.check_inside(path)?;
        let target = self.path.join(path);
        if self.write(path, contents).is_ok() && std::fs::read(&target).is_ok_and(|c| c == contents)
        {
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_file(&target);
        std::fs::write(&target, contents)
            .with_context(|| format!("cannot restore {}", target.display()))
    }

    /// Fails when a dir on the way to `path` is a symlink, which could lead outside the
    /// worktree.
    fn check_inside(&self, path: &str) -> anyhow::Result<()> {
        let mut dir = self.path.clone();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            dir.push(part);
            if dir.is_symlink() {
                anyhow::bail!(
                    "refusing to write {path}: {} is a symlink out of the worktree",
                    dir.display()
                );
            }
        }
        Ok(())
    }

    /// Copies `paths` from `tree` into this worktree, removing those that `tree` lacks.
    pub fn overlay(&self, git: &Git, tree: &str, paths: &[&str]) -> anyhow::Result<()> {
        for path in paths {
            match git.read_blob(tree, path)? {
                Some(contents) => self.write(path, &contents)?,
                None => {
                    self.check_inside(path)?;
                    let _ = std::fs::remove_file(self.path.join(path));
                }
            }
        }
        Ok(())
    }
}

/// Symlinks each runner's dependency dirs (such as `.venv` or `node_modules`) from the
/// checkout into the worktree when the tree does not contain them. The repo root's
/// `node_modules` is linked too, for workspaces that hoist dependencies.
fn link_deps(root: &Path, dest: &Path, runners: &[RunnerConfig]) -> anyhow::Result<()> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for runner in runners {
        for dep in &runner.deps {
            candidates.push(Path::new(&runner.cwd).join(dep));
        }
    }
    candidates.push(PathBuf::from("node_modules"));
    for rel in candidates {
        let rel = rel.strip_prefix(".").unwrap_or(&rel).to_path_buf();
        let source = root.join(&rel);
        let target = dest.join(&rel);
        if !source.exists() || target.exists() || target.is_symlink() {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&source, &target)
            .with_context(|| format!("cannot link {} to {}", target.display(), source.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_recovers_a_file_a_test_made_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let wt = Worktree {
            path: dir.path().to_path_buf(),
        };
        wt.write("a.py", b"mutant").unwrap();
        std::fs::set_permissions(
            dir.path().join("a.py"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();

        wt.restore("a.py", b"original").unwrap();

        assert_eq!(std::fs::read(dir.path().join("a.py")).unwrap(), b"original");
    }
}
