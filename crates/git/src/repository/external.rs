use super::*;
use crate::status::{UnmergedStatus, UnmergedStatusCode};
use std::sync::atomic::Ordering as AtomicOrdering;
use vcs_provider::{ChangeKind, Client};

pub struct ExternalRepository {
    pub client: Client,
    root: PathBuf,
    trusted: AtomicBool,
    error: Mutex<Option<String>>,
    history: Arc<Mutex<HashMap<Oid, CommitData>>>,
    revisions: Mutex<HashMap<Oid, String>>,
}

impl ExternalRepository {
    pub fn new(client: Client) -> Self {
        Self {
            root: PathBuf::from(&client.repository.root),
            client,
            trusted: AtomicBool::new(true),
            error: Mutex::new(None),
            history: Default::default(),
            revisions: Default::default(),
        }
    }

    fn oid(&self, revision: &str) -> Result<Oid> {
        let oid = if matches!(revision.len(), 40 | 64) {
            revision.parse::<Oid>().ok()
        } else {
            None
        };
        let oid = match oid {
            Some(oid) => oid,
            None => {
                // The native history UI uses Oids; the protocol keeps VCS identifiers opaque.
                let mut bytes = [0u8; 20];
                bytes[..4].copy_from_slice(b"vcs!");
                bytes[4..].copy_from_slice(
                    Uuid::new_v5(&Uuid::NAMESPACE_OID, revision.as_bytes()).as_bytes(),
                );
                Oid::from_bytes(&bytes)?
            }
        };
        let mut revisions = self.revisions.lock();
        if let Some(previous) = revisions.get(&oid) {
            anyhow::ensure!(
                previous == revision,
                "provider revision identifier collision"
            );
        }
        revisions.insert(oid, revision.to_owned());
        Ok(oid)
    }

    fn revision(&self, name: &str) -> Result<String> {
        if name == "HEAD" {
            return self
                .client
                .snapshot()
                .revision
                .context("repository has no current revision");
        }
        if let Ok(oid) = name.parse::<Oid>()
            && let Some(revision) = self.revisions.lock().get(&oid)
        {
            return Ok(revision.clone());
        }
        Ok(name.to_owned())
    }

    fn cache_commit(&self, commit: vcs_provider::HistoryCommit) -> Result<CommitData> {
        let data = CommitData {
            sha: self.oid(&commit.id)?,
            parents: commit
                .parents
                .iter()
                .map(|parent| self.oid(parent))
                .collect::<Result<_>>()?,
            author_name: commit.author_name.into(),
            author_email: commit.author_email.into(),
            commit_timestamp: commit.timestamp,
            subject: commit
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
                .into(),
            message: commit.message.into(),
        };
        self.history.lock().insert(data.sha, data.clone());
        Ok(data)
    }

    async fn history_for_source(&self, source: LogSource) -> Result<Vec<CommitData>> {
        ensure_trusted(self)?;
        let mut revision = self.revision("HEAD")?;
        let path = match source {
            LogSource::All => None,
            LogSource::Branch(branch) => {
                anyhow::ensure!(
                    self.client
                        .snapshot()
                        .branch
                        .as_deref()
                        .unwrap_or(&self.client.repository.label)
                        == branch.as_ref(),
                    "only current-branch provider history is supported"
                );
                None
            }
            LogSource::Sha(oid) => {
                revision = self.revision(&oid.to_string())?;
                None
            }
            LogSource::Path(path) => Some(path.to_string()),
        };
        let history = self
            .client
            .history(
                &revision,
                path.as_deref(),
                vcs_provider::MAX_HISTORY_COMMITS,
            )
            .await?;
        history
            .commits
            .into_iter()
            .map(|commit| self.cache_commit(commit))
            .collect()
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
            ensure_trusted(self)?;
            if !self.client.supports_history {
                anyhow::ensure!(commit == "HEAD", "provider does not support history");
                return Ok(CommitDetails {
                    sha: self.client.snapshot().revision.unwrap_or_default().into(),
                    ..Default::default()
                });
            }
            let revision = self.revision(&commit)?;
            let oid = self.oid(&revision)?;
            let cached = self.history.lock().get(&oid).cloned();
            let data = match cached {
                Some(data) => data,
                None => self.cache_commit(self.client.commit_details(&revision).await?)?,
            };
            Ok(CommitDetails {
                sha: data.sha.to_string().into(),
                message: data.message,
                commit_timestamp: data.commit_timestamp,
                author_email: data.author_email,
                author_name: data.author_name,
            })
        }
        .boxed()
    }
    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        async move {
            ensure_trusted(self)?;
            let revision = self.client.snapshot().revision;
            revs.iter()
                .map(|name| {
                    if name == "HEAD" {
                        if self.client.supports_history {
                            revision
                                .as_deref()
                                .map(|revision| self.oid(revision).map(|oid| oid.to_string()))
                                .transpose()
                        } else {
                            Ok(revision.clone())
                        }
                    } else if let Ok(oid) = name.parse::<Oid>() {
                        Ok(self
                            .revisions
                            .lock()
                            .contains_key(&oid)
                            .then(|| oid.to_string()))
                    } else {
                        Ok(None)
                    }
                })
                .collect()
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
        commit: String,
        _ignore_shallow_boundary: bool,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>> {
        async move {
            ensure_trusted(self)?;
            let revision = self.revision(&commit)?;
            let changes = self.client.commit_changes(&revision).await?;
            let mut files = Vec::with_capacity(changes.len());
            let mut bytes = 0usize;
            for change in changes {
                let contents = async {
                    let old_content = match change.base.as_deref() {
                        Some(reference) => Some(self.client.read_content(reference).await?),
                        None => None,
                    };
                    let new_content = match change.target.as_deref() {
                        Some(reference) => Some(self.client.read_content(reference).await?),
                        None => None,
                    };
                    anyhow::Ok((old_content, new_content))
                }
                .await;
                let (mut old_content, mut new_content, mut omitted_reason) = match contents {
                    Ok((old_content, new_content)) => (old_content, new_content, None),
                    Err(error) if vcs_provider::is_size_limit_error(&error) => (
                        None,
                        None,
                        Some("File exceeds the provider's content size limit".to_owned()),
                    ),
                    Err(error) => {
                        return Err(error.context(format!("loading commit file {}", change.path)));
                    }
                };
                let file_bytes = old_content.as_ref().map_or(0, Vec::len)
                    + new_content.as_ref().map_or(0, Vec::len);
                if bytes + file_bytes > 128 * 1024 * 1024 {
                    omitted_reason =
                        Some("Commit diff exceeds the 128 MiB content limit".to_owned());
                }
                if omitted_reason.is_some() {
                    // Preserve side presence for added/deleted status without pretending
                    // the placeholder is the file's historical content.
                    old_content = change.base.as_ref().map(|_| Vec::new());
                    new_content = change.target.as_ref().map(|_| Vec::new());
                } else {
                    bytes += file_bytes;
                }
                let is_binary = old_content
                    .as_ref()
                    .is_some_and(|value| is_binary_content(value))
                    || new_content
                        .as_ref()
                        .is_some_and(|value| is_binary_content(value));
                files.push(CommitFile {
                    path: RepoPath::from_rel_path(RelPath::from_unix_str(&change.path)?),
                    old_content,
                    new_content,
                    is_binary,
                    omitted_reason,
                });
            }
            Ok(CommitDiff {
                files,
                is_shallow_boundary: false,
            })
        }
        .boxed()
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
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        async move {
            let mut commits = self.history_for_source(log_source).await?;
            if log_order == LogOrder::ReverseChronological {
                commits.reverse();
            }
            let commits = commits
                .into_iter()
                .map(|commit| {
                    Arc::new(InitialGraphCommitData {
                        sha: commit.sha,
                        parents: commit.parents,
                        ref_names: Vec::new(),
                    })
                })
                .collect();
            request_tx
                .send(commits)
                .await
                .context("history receiver closed")?;
            Ok(())
        }
        .boxed()
    }
    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        async move {
            for commit in self.history_for_source(log_source).await? {
                let text = format!(
                    "{} {} {} {}",
                    self.revision(&commit.sha.to_string())?,
                    commit.author_name,
                    commit.author_email,
                    commit.message
                );
                let matches = if search_args.case_sensitive {
                    text.contains(search_args.query.as_ref())
                } else {
                    text.to_lowercase()
                        .contains(&search_args.query.to_lowercase())
                };
                if matches {
                    request_tx
                        .send(commit.sha)
                        .await
                        .context("history search receiver closed")?;
                }
            }
            Ok(())
        }
        .boxed()
    }
    fn file_history_changed_files(
        &self,
        _paths: Vec<RepoPath>,
        _commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        async { bail!("operation unavailable: external VCS provider is read-only") }.boxed()
    }
    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        ensure_trusted(self)?;
        anyhow::ensure!(
            self.client.supports_history,
            "provider does not support history"
        );
        Ok(CommitDataReader::from_cache(self.history.clone()))
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
