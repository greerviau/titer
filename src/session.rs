//! The state every tier shares: the checkout, its config, the base, head, and changed lines.

use std::path::Path;

use anyhow::Context;

use crate::config::Config;
use crate::diff::ChangeSet;
use crate::git::Git;
use crate::tmp::RunDir;

pub struct Session {
    pub git: Git,
    pub config: Config,
    pub run_dir: RunDir,
    pub base_ref: String,
    /// The merge-base of HEAD and the base ref.
    pub base_sha: String,
    /// The tree object of the working-tree snapshot.
    pub head_tree: String,
    pub changes: ChangeSet,
}

impl Session {
    /// Opens the opted-in checkout that contains `cwd` and snapshots it.
    pub fn open(cwd: &Path, base: Option<&str>) -> anyhow::Result<Session> {
        let git = Git::discover(cwd)?;
        let config = Config::load(git.root())?;
        let run_dir = RunDir::create()?;
        let base_ref = git.base_ref(base, config.base.as_deref())?;
        let base_sha = git.merge_base(&base_ref)?;
        let head_tree = git
            .snapshot(&run_dir.join("snapshot.index"))
            .context("cannot snapshot the working tree")?;
        let changes = ChangeSet::parse(&git.diff(&base_sha, &head_tree)?);
        Ok(Session {
            git,
            config,
            run_dir,
            base_ref,
            base_sha,
            head_tree,
            changes,
        })
    }
}
