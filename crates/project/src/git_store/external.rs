use super::*;
use git::repository::ExternalRepository;

impl GitStore {
    pub(super) fn start_external_providers(&mut self, cx: &mut Context<Self>) {
        let GitStoreState::Local { fs, .. } = &self.state else {
            return;
        };
        let fs = fs.clone();
        let worktrees = self.worktree_store.read(cx).worktrees().collect::<Vec<_>>();
        for worktree in worktrees {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            let root = worktree.abs_path();
            let settings = ProjectSettings::get(
                Some(SettingsLocation {
                    worktree_id,
                    path: RelPath::empty(),
                }),
                cx,
            );
            let Some(configuration) = settings.vcs_provider.clone() else {
                continue;
            };
            if !worktree.is_local() || self.external_starts.contains_key(&worktree_id) {
                continue;
            }
            let trusted = TrustedWorktrees::try_get_global(cx).is_some_and(|trusted| {
                trusted.update(cx, |trusted, cx| {
                    trusted.can_trust(&self.worktree_store, worktree_id, cx)
                })
            });
            if !trusted {
                continue;
            }
            let fs = fs.clone();
            let task = cx.spawn(async move |this, cx| {
                let timeout = Duration::from_millis(
                    configuration
                        .request_timeout_ms
                        .unwrap_or(30_000)
                        .clamp(100, 300_000),
                );
                let interval =
                    Duration::from_millis(configuration.poll_interval_ms.unwrap_or(2000).max(250));
                let client = vcs_provider::Client::start(
                    &configuration.command,
                    &configuration.args,
                    &configuration.env,
                    &root,
                    timeout,
                )
                .await;
                match client {
                    Ok(client) => {
                        this.update(cx, |this, cx| {
                            if this
                                .worktree_store
                                .read(cx)
                                .worktree_for_id(worktree_id, cx)
                                .is_none()
                            {
                                return;
                            }
                            let trusted =
                                TrustedWorktrees::try_get_global(cx).is_some_and(|trusted| {
                                    trusted.update(cx, |trusted, cx| {
                                        trusted.can_trust(&this.worktree_store, worktree_id, cx)
                                    })
                                });
                            if trusted {
                                this.register_external_repository(
                                    worktree_id,
                                    Arc::new(ExternalRepository::new(client)),
                                    fs,
                                    interval,
                                    cx,
                                );
                            }
                        })
                        .log_err();
                    }
                    Err(error) => log::error!("VCS provider for {}: {error:#}", root.display()),
                }
            });
            self.external_starts.insert(worktree_id, task);
        }
    }

    fn register_external_repository(
        &mut self,
        worktree_id: WorktreeId,
        backend: Arc<ExternalRepository>,
        fs: Arc<dyn Fs>,
        interval: Duration,
        cx: &mut Context<Self>,
    ) {
        let GitStoreState::Local {
            next_repository_id,
            downstream,
            ..
        } = &self.state
        else {
            return;
        };
        let updates_tx = downstream
            .as_ref()
            .map(|downstream| downstream.updates_tx.clone());
        let id = RepositoryId(next_repository_id.fetch_add(1, atomic::Ordering::Release));
        let root: Arc<Path> = backend.path().into();
        // Provider selection is explicit for this scope. Replace an already discovered
        // native repository so two backends cannot compete for the same files.
        let replaced = self
            .repositories
            .iter()
            .filter_map(|(id, repository)| {
                (repository.read(cx).work_directory_abs_path == root).then_some(*id)
            })
            .collect::<Vec<_>>();
        for replaced in replaced {
            if let Some(updates_tx) = &updates_tx {
                updates_tx
                    .unbounded_send(DownstreamUpdate::RemoveRepository(replaced))
                    .log_err();
            }
            self.repositories.remove(&replaced);
            self.worktree_ids.remove(&replaced);
            self.display_diffs.remove(&replaced);
            if self.active_repo_id == Some(replaced) {
                self.active_repo_id = None;
            }
        }
        let git_store = cx.weak_entity();
        let repository = cx.new(|cx| {
            Repository::external(id, root, backend, fs, interval, git_store, updates_tx, cx)
        });
        self._subscriptions
            .push(cx.subscribe(&repository, Self::on_repository_event));
        self._subscriptions
            .push(cx.subscribe(&repository, Self::on_jobs_updated));
        self.repositories.insert(id, repository);
        self.update_diff_operations_for_repository(id, cx);
        self.worktree_ids
            .insert(id, HashSet::from_iter([worktree_id]));
        cx.emit(GitStoreEvent::RepositoryAdded);
        self.refresh_diff_base_for_repo(id, cx);
        if self.active_repo_id.is_none() {
            self.active_repo_id = Some(id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
        }
    }

    pub(super) fn update_diff_operations_for_repository(
        &self,
        id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get(&id) else {
            return;
        };
        let read_only = repository.read(cx).is_read_only();
        if let Some(project) = self.project.clone() {
            for (buffer_id, state) in &self.diffs {
                if !self
                    .repository_and_path_for_buffer_id(*buffer_id, cx)
                    .is_some_and(|(repository, _)| repository.read(cx).id == id)
                {
                    continue;
                }
                let state = state.read(cx);
                for (diff, kind) in [
                    (state.unstaged_diff(), DiffKind::Unstaged),
                    (state.staged_diff(), DiffKind::Staged),
                    (state.uncommitted_diff(), DiffKind::Uncommitted),
                ] {
                    if let Some(diff) = diff {
                        diff.update(cx, |diff, _| {
                            diff.set_operations(Arc::new(GitDiffOperations {
                                project: project.clone(),
                                kind,
                                read_only,
                            }))
                        });
                    }
                }
            }
        }
    }
}

impl Repository {
    fn external(
        id: RepositoryId,
        root: Arc<Path>,
        backend: Arc<ExternalRepository>,
        fs: Arc<dyn Fs>,
        interval: Duration,
        git_store: WeakEntity<GitStore>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Metadata paths are inert placeholders for the existing snapshot type. No
        // filesystem repository is opened; every VCS operation goes through the backend.
        let mut snapshot = RepositorySnapshot::empty(
            id,
            root.clone(),
            Some(root.clone()),
            Some(root.clone()),
            Some(root),
            PathStyle::local(),
        );
        snapshot.is_read_only = true;
        let state = LocalRepositoryState {
            backend: backend.clone(),
            fs,
            environment: Arc::default(),
        };
        let worker_state = Task::ready(Ok(state.clone())).shared();
        let (job_sender, worker_task) = Self::spawn_local_git_worker(worker_state, cx);
        let mut repository = Self {
            this: cx.weak_entity(),
            git_store,
            snapshot,
            external_backend: Some(backend.clone()),
            _provider_task: Task::ready(()),
            unshallow_state: UnshallowState::default(),
            pending_ops: Default::default(),
            repository_state: Task::ready(Ok(RepositoryState::Local(state))).shared(),
            _worker_task: worker_task,
            commit_message_buffer: None,
            askpass_delegates: Default::default(),
            paths_needing_status_update: Default::default(),
            latest_askpass_id: 0,
            job_sender,
            job_id: 0,
            active_jobs: Default::default(),
            job_debug_queue: job_debug_queue::GitJobDebugQueue::new(),
            initial_graph_data: Default::default(),
            commit_data: Default::default(),
            commit_data_handler: CommitDataHandlerState::Closed,
        };
        repository.schedule_scan(updates_tx, cx);
        repository._provider_task = cx.spawn(async move |this, cx| {
            let mut previous_snapshot = backend.client.snapshot().snapshot;
            let mut previous_error = None;
            loop {
                cx.background_executor().timer(interval).await;
                if !backend.is_trusted() {
                    break;
                }
                let result = backend.refresh().await;
                let error = result.as_ref().err().map(ToString::to_string);
                let snapshot = backend.client.snapshot().snapshot;
                if snapshot != previous_snapshot || error != previous_error {
                    if let Some(error) = &error {
                        log::error!("VCS provider: {error}");
                    }
                    if this
                        .update(cx, |this, cx| {
                            let updates_tx =
                                this.git_store.upgrade().and_then(|store| {
                                    match &store.read(cx).state {
                                        GitStoreState::Local {
                                            downstream: Some(downstream),
                                            ..
                                        } => Some(downstream.updates_tx.clone()),
                                        _ => None,
                                    }
                                });
                            this.schedule_scan(updates_tx, cx);
                            if result.is_ok() {
                                this.reload_buffer_diff_bases(cx);
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                    previous_snapshot = snapshot;
                    previous_error = error;
                }
            }
        });
        cx.subscribe_self(Self::handle_subscribe_self).detach();
        repository
    }

    pub fn is_read_only(&self) -> bool {
        self.snapshot.is_read_only
    }

    pub(super) fn stop_external_provider(&mut self) {
        if let Some(backend) = &self.external_backend {
            self._provider_task = Task::ready(());
            backend.set_trusted(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use util::rel_path::rel_path;

    #[gpui::test]
    async fn external_provider_requires_trust_and_restarts(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            crate::trusted_worktrees::init(Default::default(), cx);
        });
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(root, json!({"hello.txt":"working\n"})).await;
        let project = Project::test_with_worktree_trust(fs.clone(), [root], cx).await;
        let (store, worktree_store, worktree_id, trusted) = project.read_with(cx, |project, cx| {
            (
                project.git_store().clone(),
                project.worktree_store(),
                project.worktrees(cx).next().unwrap().read(cx).id(),
                TrustedWorktrees::try_get_global(cx).unwrap(),
            )
        });
        trusted.update(cx, |trusted, cx| {
            trusted.restrict(
                worktree_store.downgrade(),
                HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                cx,
            )
        });
        let configuration = json!({"vcs_provider": {"command":"python3", "args":[root.join("../vcs_provider/tests/mock_provider.py"), "text"], "poll_interval_ms":60000}}).to_string();
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|settings, cx| {
                settings.set_user_settings(&configuration, cx).unwrap();
            })
        });
        cx.executor().run_until_parked();
        store.read_with(cx, |store, cx| {
            assert!(store.external_starts.is_empty());
            assert!(store.repositories.is_empty());
            assert!(
                ProjectSettings::get(
                    Some(SettingsLocation {
                        worktree_id,
                        path: RelPath::empty()
                    }),
                    cx
                )
                .vcs_provider
                .is_some()
            );
        });
        assert!(
            store
                .read_with(cx, |store, cx| store.git_init(
                    root.into(),
                    "main".into(),
                    cx
                ))
                .await
                .is_err()
        );
        for _ in 0..2 {
            let (sender, receiver) = oneshot::channel();
            let mut sender = Some(sender);
            let _subscription = project.update(cx, |_, cx| {
                cx.subscribe(&store, move |_, _, event, _| {
                    if matches!(event, GitStoreEvent::RepositoryAdded)
                        && let Some(sender) = sender.take()
                    {
                        sender.send(()).unwrap();
                    }
                })
            });
            trusted.update(cx, |trusted, cx| {
                trusted.trust(
                    &worktree_store,
                    HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                    cx,
                )
            });
            let completed = futures::future::select(
                receiver,
                cx.executor().timer(Duration::from_secs(5)).boxed(),
            )
            .await;
            assert!(
                matches!(completed, futures::future::Either::Left((Ok(()), _))),
                "provider discovery timed out"
            );
            let repository = store.read_with(cx, |store, cx| {
                assert_eq!(store.repositories.len(), 1);
                let repository = store.active_repository().unwrap();
                assert!(repository.read(cx).is_trusted());
                assert!(repository.read(cx).is_read_only());
                repository
            });
            trusted.update(cx, |trusted, cx| {
                trusted.restrict(
                    worktree_store.downgrade(),
                    HashSet::from_iter([PathTrust::Worktree(worktree_id)]),
                    cx,
                )
            });
            cx.executor().run_until_parked();
            assert!(!repository.read_with(cx, |repository, _| repository.is_trusted()));
            store.read_with(cx, |store, _| {
                assert!(store.repositories.is_empty());
                assert!(store.external_starts.is_empty());
            });
        }
        assert!(fs.metadata(&root.join(".git")).await.unwrap().is_none());
    }

    #[gpui::test]
    async fn external_provider_drives_native_status_and_read_only_diffs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let refreshed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider_refreshed = refreshed.clone();
        let client = vcs_provider::Client::mock(
            vcs_provider::RepositoryInfo { id: "mock".into(), root: root.to_string_lossy().into_owned(), label: "Mock VCS".into() },
            serde_json::from_value(json!({"snapshot":"1", "revision":"revision", "branch":"main", "changes":[{"path":"hello.txt", "status":"modified", "stagedStatus":"modified"}]})).unwrap(),
            move |method, parameters| match method {
                "repository/status" => Ok(json!({"snapshot": if provider_refreshed.load(atomic::Ordering::Acquire) { "2" } else { "1" }, "revision":"revision", "branch":"main", "changes":[{"path":"hello.txt", "status":"modified", "stagedStatus":"modified"}]})),
                "repository/comparison" => Ok(parameters["paths"].as_array().unwrap().iter().map(|path| json!({"path":path, "base": if parameters["snapshot"] == "2" { "base-2" } else { "base" }, "index":"index"})).collect()),
                "repository/readContent" => Ok(json!({"encoding":"base64", "data": if parameters["content"] == "base-2" { "dXBkYXRlZAo=" } else if parameters["content"] == "base" { "YmFzZQo=" } else { "aW5kZXgK" }})),
                _ => bail!("unexpected mock method: {method}"),
            }
        ).unwrap();
        let backend = Arc::new(ExternalRepository::new(client));
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            root,
            json!({"hello.txt": "working copy\n", "new.txt": "new\n", "clean.txt": "base\n"}),
        )
        .await;
        let project = Project::test(fs.clone(), [root], cx).await;
        let (git_store, worktree_id) = project.read_with(cx, |project, cx| {
            (
                project.git_store().clone(),
                project.worktrees(cx).next().unwrap().read(cx).id(),
            )
        });
        git_store.update(cx, |store, cx| {
            store.register_external_repository(
                worktree_id,
                backend.clone(),
                fs,
                Duration::from_secs(60),
                cx,
            )
        });
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = git_store.read_with(cx, |store, cx| {
            let (repository, path) = store
                .repository_and_path_for_project_path(
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("hello.txt").into(),
                    },
                    cx,
                )
                .unwrap();
            assert_eq!(
                repository.read(cx).status_for_path(&path).unwrap().status,
                FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Modified,
                    worktree_status: StatusCode::Modified
                })
            );
            assert!(repository.read(cx).is_read_only());
            assert!(repository.read(cx).is_trusted());
            repository
        });
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer((worktree_id, rel_path("hello.txt")), cx)
            })
            .await
            .unwrap();
        let diff = project
            .update(cx, |project, cx| {
                project.open_uncommitted_diff(buffer.clone(), cx)
            })
            .await
            .unwrap();
        diff.read_with(cx, |diff, cx| {
            assert_eq!(diff.snapshot(cx).base_text().text(), "base\n");
            let operations = diff.operations().unwrap();
            assert!(!operations.supports_staging());
            assert!(!operations.supports_unstaging());
            assert!(!operations.supports_restore());
        });
        refreshed.store(true, atomic::Ordering::Release);
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.executor().run_until_parked();
        repository
            .update(cx, |repository, _| repository.barrier())
            .await
            .unwrap();
        cx.executor().run_until_parked();
        diff.read_with(cx, |diff, cx| {
            assert_eq!(diff.snapshot(cx).base_text().text(), "updated\n")
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "editable\n")], None, cx)
        });
        assert!(buffer.read_with(cx, |buffer, _| buffer.text().starts_with("editable\n")));
        assert!(
            repository
                .update(cx, |repository, cx| repository.stage_all(cx))
                .await
                .is_err()
        );
        let ignore_write = repository.update(cx, |repository, _| {
            repository.add_path_to_gitignore(&git::repository::repo_path("hello.txt"), false)
        });
        assert!(ignore_write.await.unwrap().is_err());
        assert!(smol::block_on(backend.stage_paths(Vec::new(), Arc::default())).is_err());
        backend.set_trusted(false);
        assert!(smol::block_on(backend.refresh()).is_err());
    }
}
