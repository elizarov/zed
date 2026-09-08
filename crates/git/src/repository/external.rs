use super::*;
use crate::status::{UnmergedStatus, UnmergedStatusCode};
use std::sync::atomic::Ordering as AtomicOrdering;
use vcs_provider::{ChangeKind, Client};

pub struct ExternalRepository {
    pub client: Client,
    root: PathBuf,
    trusted: AtomicBool,
    error: Mutex<Option<String>>,
}

impl ExternalRepository {
    pub fn new(client: Client) -> Self {
        Self {
            root: PathBuf::from(&client.repository.root),
            client,
            trusted: AtomicBool::new(true),
            error: Mutex::new(None),
        }
    }

    pub async fn refresh(&self) -> Result<()> {
        ensure_trusted(self)?;
        let result = self.client.refresh().await;
        *self.error.lock() = result.as_ref().err().map(ToString::to_string);
        result
    }
}

fn ensure_trusted(repository: &ExternalRepository) -> Result<()> {
    anyhow::ensure!(
        repository.is_trusted(),
        "VCS provider workspace is not trusted"
    );
    Ok(())
}

fn status_code(kind: ChangeKind) -> StatusCode {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => StatusCode::Added,
        ChangeKind::Modified | ChangeKind::Conflicted => StatusCode::Modified,
        ChangeKind::Deleted => StatusCode::Deleted,
        // The current native change list excludes these Git status codes from
        // summaries/diffs. Keep their baselines, but use the visible modified kind.
        ChangeKind::Renamed | ChangeKind::Copied | ChangeKind::TypeChanged => StatusCode::Modified,
        ChangeKind::Unchanged | ChangeKind::Ignored => StatusCode::Unmodified,
    }
}

fn file_status(change: &vcs_provider::Change) -> FileStatus {
    match change.status {
        ChangeKind::Untracked if change.staged_status == ChangeKind::Unchanged => {
            FileStatus::Untracked
        }
        ChangeKind::Ignored => FileStatus::Ignored,
        ChangeKind::Conflicted => FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        }),
        _ => FileStatus::Tracked(TrackedStatus {
            index_status: status_code(change.staged_status),
            worktree_status: status_code(change.status),
        }),
    }
}

impl GitRepository for ExternalRepository {
    fn is_read_only(&self) -> bool {
        true
    }
    fn path(&self) -> PathBuf {
        self.root.clone()
    }
    fn main_repository_path(&self) -> PathBuf {
        self.root.clone()
    }
    fn set_trusted(&self, trusted: bool) {
        self.trusted.store(trusted, AtomicOrdering::Release);
        if !trusted {
            self.client.stop();
        }
    }
    fn is_trusted(&self) -> bool {
        self.trusted.load(AtomicOrdering::Acquire)
    }
    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let result = (|| {
            ensure_trusted(self)?;
            let mut entries = self
                .client
                .snapshot()
                .changes
                .iter()
                .map(|change| {
                    Ok((
                        RepoPath::from_rel_path(RelPath::from_unix_str(&change.path)?),
                        file_status(change),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            entries.retain(|(path, _)| {
                path_prefixes.is_empty()
                    || path_prefixes
                        .iter()
                        .any(|prefix| path.as_ref().starts_with(prefix.as_ref()))
            });
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            Ok(GitStatus {
                entries: entries.into(),
            })
        })();
        Task::ready(result)
    }
    fn load_revisions(
        &self,
        revisions: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>> {
        async move {
            ensure_trusted(self)?;
            let mut paths = Vec::new();
            let mut staged = Vec::new();
            for revision in revisions {
                if let Some(path) = revision.strip_prefix("HEAD:") {
                    paths.push(path.to_owned());
                    staged.push(false);
                } else if let Some(path) = revision.strip_prefix(':') {
                    paths.push(path.to_owned());
                    staged.push(true);
                } else {
                    bail!(
                        "arbitrary revision reads are not supported by this VCS provider prototype"
                    );
                }
            }
            self.client.contents(&paths, &staged).await
        }
        .boxed()
    }
    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let snapshot = self.client.snapshot();
        let branch = snapshot
            .branch
            .unwrap_or_else(|| self.client.repository.label.clone());
        let result = BranchesScanResult {
            branches: vec![Branch {
                is_head: true,
                ref_name: branch.into(),
                upstream: None,
                most_recent_commit: None,
            }],
            error: self.error.lock().clone().map(Into::into),
        };
        async move { Ok(result) }.boxed()
    }
    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        async move {
            anyhow::ensure!(
                commit == "HEAD",
                "history is unavailable for external VCS providers"
            );
            let snapshot = self.client.snapshot();
            Ok(CommitDetails {
                sha: snapshot.revision.unwrap_or_default().into(),
                ..Default::default()
            })
        }
        .boxed()
    }
    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let revision = self.client.snapshot().revision;
        async move {
            Ok(revs
                .iter()
                .map(|name| {
                    if name == "HEAD" {
                        revision.clone()
                    } else {
                        None
                    }
                })
                .collect())
        }
        .boxed()
    }
    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        async { HashMap::default() }.boxed()
    }
    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        async { Ok(Vec::new()) }.boxed()
    }
    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        async { Ok(GitStash::default()) }.boxed()
    }
    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        async { None }.boxed()
    }
    fn diff_stat(
        &self,
        _diff: DiffStatType,
        _path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>> {
        async { Ok(Default::default()) }.boxed()
    }
    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        async { Ok(None) }.boxed()
    }
    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        async { Ok(Vec::new()) }.boxed()
    }
    fn load_blob_content(&self, _oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn set_index_text(
        &self,
        _path: RepoPath,
        _content: Option<Vec<u8>>,
        _env: Arc<HashMap<String, String>>,
        _is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn change_branch(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn create_branch(
        &self,
        _name: String,
        _base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn rename_branch(&self, _branch: String, _new_name: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn delete_branch(
        &self,
        _is_remote: bool,
        _name: String,
        _force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn worktree_created_at(
        &self,
        _worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn create_worktree(
        &self,
        _target: CreateWorktreeTarget,
        _path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn checkout_branch_in_worktree(
        &self,
        _branch_name: String,
        _worktree_path: PathBuf,
        _create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn remove_worktree(&self, _path: PathBuf, _force: bool) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn rename_worktree(&self, _old_path: PathBuf, _new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn reset(
        &self,
        _commit: String,
        _mode: ResetMode,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn checkout_files(
        &self,
        _commit: String,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn load_commit(
        &self,
        _commit: String,
        _ignore_shallow_boundary: bool,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn blame(
        &self,
        _path: RepoPath,
        _content: Rope,
        _line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn blame_at_revision(
        &self,
        _path: RepoPath,
        _revision: Oid,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn unstage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn run_hook(
        &self,
        _hook: RunHook,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn commit(
        &self,
        _message: SharedString,
        _name_and_email: Option<(SharedString, SharedString)>,
        _options: CommitOptions,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stash_paths(
        &self,
        _paths: Vec<RepoPath>,
        _message: Option<String>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stash_staged(
        &self,
        _message: Option<String>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stash_pop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stash_apply(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn stash_drop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn push(
        &self,
        _branch_name: String,
        _remote_branch_name: String,
        _upstream_name: String,
        _options: Option<PushOptions>,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn pull(
        &self,
        _branch_name: Option<String>,
        _upstream_name: String,
        _rebase: bool,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn fetch(
        &self,
        _fetch_options: FetchOptions,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn get_push_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn get_branch_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn remove_remote(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn create_remote(&self, _name: String, _url: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn diff(&self, _diff: DiffType) -> BoxFuture<'_, Result<String>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn restore_checkpoint(
        &self,
        _checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn restore_archive_checkpoint(
        &self,
        _staged_sha: String,
        _unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn compare_checkpoints(
        &self,
        _left: GitRepositoryCheckpoint,
        _right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn diff_checkpoints(
        &self,
        _base_checkpoint: GitRepositoryCheckpoint,
        _target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn initial_graph_data(
        &self,
        _log_source: LogSource,
        _log_order: LogOrder,
        _request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn search_commits(
        &self,
        _log_source: LogSource,
        _search_args: SearchCommitArgs,
        _request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn file_history_changed_files(
        &self,
        _paths: Vec<RepoPath>,
        _commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        bail!("operation unavailable: external VCS provider is read-only")
    }
    fn update_ref(&self, _ref_name: String, _commit: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn delete_ref(&self, _ref_name: String) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_changes_are_visible_to_native_diff_and_tree_consumers() {
        for kind in [
            ChangeKind::Renamed,
            ChangeKind::Copied,
            ChangeKind::TypeChanged,
        ] {
            let status = file_status(&vcs_provider::Change {
                path: "new.txt".into(),
                status: ChangeKind::Unchanged,
                staged_status: kind,
                original_path: Some("old.txt".into()),
            });
            assert!(status.has_changes());
            assert_eq!(status.summary().index.modified, 1);
        }
    }
}
