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

    /// Writes `contents` to the repo-relative `path`, replacing what is there.
    pub fn write(&self, path: &str, contents: &[u8]) -> anyhow::Result<()> {
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

    /// Copies `paths` from `tree` into this worktree, removing those that `tree` lacks.
    pub fn overlay(&self, git: &Git, tree: &str, paths: &[&str]) -> anyhow::Result<()> {
        for path in paths {
            match git.read_blob(tree, path)? {
                Some(contents) => self.write(path, &contents)?,
                None => {
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
