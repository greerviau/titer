//! Read-only access to the user's git checkout.
//!
//! Every command here leaves the checkout's working tree, index, stash, refs, and worktree
//! list unchanged. The snapshot writes objects to the object database, which git status does
//! not show.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::process;

#[derive(Debug, Clone)]
pub struct Git {
    root: PathBuf,
}

impl Git {
    /// Opens the checkout that contains `cwd`.
    pub fn discover(cwd: &Path) -> anyhow::Result<Git> {
        let mut cmd = base_command(cwd);
        cmd.args(["rev-parse", "--show-toplevel"]);
        let out = process::output(cmd).context("cannot run git")?;
        if !out.status.success() {
            bail!(
                "not a git checkout: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let root = String::from_utf8(out.stdout).context("repo path is not UTF-8")?;
        Ok(Git {
            root: PathBuf::from(root.trim()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn command(&self) -> Command {
        base_command(&self.root)
    }

    /// Runs git with `args` and returns stdout. Fails with git's stderr.
    pub fn run<I, S>(&self, args: I) -> anyhow::Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command();
        cmd.args(args);
        run_command(cmd)
    }

    pub fn run_bytes<I, S>(&self, args: I) -> anyhow::Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command();
        cmd.args(args);
        let shown = format!("{cmd:?}");
        let out = process::output(cmd).context("cannot run git")?;
        if !out.status.success() {
            bail!(
                "{shown} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Resolves the base ref name: the explicit ref, then the configured one, then the remote
    /// default branch.
    pub fn base_ref(
        &self,
        explicit: Option<&str>,
        configured: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(name) = explicit.or(configured) {
            return Ok(name.to_string());
        }
        if let Ok(head) = self.run([
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]) {
            return Ok(head.trim().to_string());
        }
        for candidate in ["origin/main", "origin/master", "main", "master"] {
            if self.rev_parse_commit(candidate).is_ok() {
                return Ok(candidate.to_string());
            }
        }
        bail!("cannot find the default branch; pass --base or set `base` in titer.toml")
    }

    pub fn rev_parse_commit(&self, rev: &str) -> anyhow::Result<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .run(["rev-parse", "--verify", "--quiet", &spec])
            .with_context(|| format!("unknown base ref {rev:?}"))?;
        Ok(out.trim().to_string())
    }

    /// The merge-base of HEAD and `base_ref`.
    pub fn merge_base(&self, base_ref: &str) -> anyhow::Result<String> {
        let base = self.rev_parse_commit(base_ref)?;
        self.rev_parse_commit("HEAD")
            .context("HEAD has no commit yet")?;
        let out = self
            .run(["merge-base", "HEAD", &base])
            .with_context(|| format!("HEAD and {base_ref} share no history"))?;
        Ok(out.trim().to_string())
    }

    /// Writes a tree object for the working tree, including untracked files that are not
    /// ignored. The user's index is copied to `scratch_index` and only the copy is updated.
    pub fn snapshot(&self, scratch_index: &Path) -> anyhow::Result<String> {
        let index = self.run(["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
        let index = PathBuf::from(index.trim());
        if index.is_file() {
            std::fs::copy(&index, scratch_index)
                .with_context(|| format!("cannot copy {}", index.display()))?;
            // Keep the index's mtime. Git's racy-entry check compares entry times with it, and
            // a fresh mtime would let a same-size edit made in the same second look clean.
            let modified = std::fs::metadata(&index)?.modified()?;
            std::fs::File::options()
                .write(true)
                .open(scratch_index)?
                .set_modified(modified)?;
        } else {
            let _ = std::fs::remove_file(scratch_index);
        }
        let mut add = self.command();
        add.env("GIT_INDEX_FILE", scratch_index)
            .args(["add", "-A", "--", "."]);
        run_command(add)?;
        let mut write = self.command();
        write.env("GIT_INDEX_FILE", scratch_index).arg("write-tree");
        Ok(run_command(write)?.trim().to_string())
    }

    /// The unified diff from `base` to `head` with no context lines, renames detected.
    pub fn diff(&self, base: &str, head: &str) -> anyhow::Result<String> {
        let bytes = self.run_bytes([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--unified=0",
            base,
            head,
        ])?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The contents of `path` in `tree`, or None when the path does not exist there.
    pub fn read_blob(&self, tree: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let spec = format!("{tree}:{path}");
        let mut cmd = self.command();
        cmd.args(["cat-file", "blob", &spec]);
        let out = process::output(cmd).context("cannot run git")?;
        Ok(out.status.success().then_some(out.stdout))
    }

    /// Every file path in `tree`, which may be a tree or a commit.
    pub fn list_files(&self, tree: &str) -> anyhow::Result<std::collections::HashSet<String>> {
        let bytes = self.run_bytes(["ls-tree", "-r", "-z", "--name-only", tree])?;
        Ok(bytes
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    /// Writes every file of `tree` under `dest`, without registering a git worktree.
    pub fn materialize(&self, tree: &str, dest: &Path, scratch_index: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dest)?;
        let mut read = self.command();
        read.env("GIT_INDEX_FILE", scratch_index)
            .args(["read-tree", tree]);
        run_command(read)?;
        let mut prefix = dest.as_os_str().to_owned();
        prefix.push("/");
        let mut checkout = self.command();
        checkout
            .env("GIT_INDEX_FILE", scratch_index)
            .args(["checkout-index", "--all", "--force", "--prefix"])
            .arg(prefix);
        run_command(checkout)?;
        let _ = std::fs::remove_file(scratch_index);
        Ok(())
    }
}

fn base_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.autocrlf=false",
        ]);
    cmd
}

fn run_command(cmd: Command) -> anyhow::Result<String> {
    let shown = format!("{cmd:?}");
    let out = process::output(cmd).context("cannot run git")?;
    if !out.status.success() {
        bail!(
            "{shown} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git output is not UTF-8")
}
