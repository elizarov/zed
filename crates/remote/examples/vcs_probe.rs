//! Read-only smoke test against a dedicated remote-server proxy over stdio.
use anyhow::{Context as _, Result, bail, ensure};
use remote::protocol::{read_message, write_message};
use rpc::proto::{self, EnvelopedMessage, RequestMessage, envelope::Payload};
use smol::{
    io::AsyncWriteExt as _,
    process::{ChildStdin, ChildStdout, Command, Stdio},
};
use std::{collections::BTreeMap, time::Duration};

struct Probe {
    input: ChildStdin,
    output: ChildStdout,
    next_id: u32,
    ack_id: Option<u32>,
    next_worktree_id: u64,
    saw_repository_loading: bool,
    repository_loading: bool,
    repositories: BTreeMap<u64, proto::UpdateRepository>,
}

impl Probe {
    async fn send(&mut self, mut message: proto::Envelope) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        message.id = id;
        message.ack_id = self.ack_id;
        write_message(&mut self.input, &mut Vec::new(), message).await?;
        self.input.flush().await?;
        Ok(id)
    }

    async fn receive(&mut self) -> Result<proto::Envelope> {
        let message = smol::future::or(read_message(&mut self.output, &mut Vec::new()), async {
            smol::Timer::after(Duration::from_secs(120)).await;
            bail!("timed out waiting for the remote server")
        })
        .await?;
        self.ack_id = Some(message.id);
        if message.responding_to.is_none() {
            match &message.payload {
                Some(
                    Payload::RemoteStarted(_)
                    | Payload::Ping(_)
                    | Payload::UpdateWorktree(_)
                    | Payload::UpdateProject(_),
                ) => {
                    self.send(proto::Ack {}.into_envelope(0, Some(message.id), None))
                        .await?;
                }
                Some(Payload::AllocateWorktreeId(_)) => {
                    let worktree_id = self.next_worktree_id;
                    self.next_worktree_id += 1;
                    self.send(
                        proto::AllocateWorktreeIdResponse { worktree_id }.into_envelope(
                            0,
                            Some(message.id),
                            None,
                        ),
                    )
                    .await?;
                }
                Some(Payload::UpdateRepositoryDiscovery(update)) => {
                    if let Some(error) = &update.error {
                        bail!("repository startup failed: {error}");
                    }
                    self.repository_loading = update.loading;
                    self.saw_repository_loading |= update.loading;
                    println!("Repository loading: {}", update.loading);
                }
                Some(Payload::UpdateRepository(update)) => {
                    let repository = self.repositories.entry(update.id).or_default();
                    let mut statuses = std::mem::take(&mut repository.updated_statuses);
                    statuses.retain(|status| {
                        !update.removed_statuses.contains(&status.repo_path)
                            && !update
                                .updated_statuses
                                .iter()
                                .any(|new| new.repo_path == status.repo_path)
                    });
                    statuses.extend(update.updated_statuses.clone());
                    *repository = update.clone();
                    repository.updated_statuses = statuses;
                }
                Some(Payload::RemoveRepository(update)) => {
                    self.repositories.remove(&update.id);
                }
                _ => {}
            }
        }
        Ok(message)
    }

    async fn request<T: RequestMessage>(&mut self, request: T) -> Result<T::Response> {
        let id = self.send(request.into_envelope(0, None, None)).await?;
        loop {
            let message = self.receive().await?;
            if message.responding_to == Some(id) {
                if let Some(Payload::Error(error)) = &message.payload {
                    bail!("{}: {}", T::NAME, error.message);
                }
                return T::Response::from_envelope(message).context("unexpected response type");
            }
        }
    }
}

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let root = arguments
        .next()
        .context("usage: vcs_probe ROOT FILE COMMAND [ARG ...]")?;
    let path = arguments.next().context("missing relative file path")?;
    let command = arguments.next().context("missing proxy command")?;
    smol::block_on(async {
        let mut child = Command::new(command)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut probe = Probe {
            input: child.stdin.take().context("missing proxy stdin")?,
            output: child.stdout.take().context("missing proxy stdout")?,
            next_id: 1,
            ack_id: None,
            next_worktree_id: 1,
            saw_repository_loading: false,
            repository_loading: false,
            repositories: BTreeMap::new(),
        };
        probe.request(proto::RemoteStarted {}).await?;
        println!("Remote server handshake complete");
        let project_id = proto::REMOTE_SERVER_PROJECT_ID;
        let worktree = probe
            .request(proto::AddWorktree {
                project_id,
                path: root,
                visible: true,
            })
            .await?;
        println!("Remote worktree opened: {}", worktree.worktree_id);
        probe
            .request(proto::TrustWorktrees {
                project_id,
                trusted_paths: vec![proto::PathTrust {
                    content: Some(proto::path_trust::Content::WorktreeId(worktree.worktree_id)),
                }],
            })
            .await?;
        loop {
            if let Some(repository) = probe
                .repositories
                .values()
                .find(|repository| repository.is_last_update)
            {
                ensure!(repository.is_read_only, "repository must be read-only");
                println!(
                    "Remote repository: {}; {} changed paths; read-only",
                    repository.abs_path,
                    repository.updated_statuses.len()
                );
                break;
            }
            probe.receive().await?;
        }
        while probe.repository_loading {
            probe.receive().await?;
        }
        ensure!(
            probe.saw_repository_loading,
            "missing repository startup notification"
        );
        let buffer = probe
            .request(proto::OpenBufferByPath {
                project_id,
                worktree_id: worktree.worktree_id,
                path,
            })
            .await?;
        let diff = probe
            .request(proto::OpenUncommittedDiff {
                project_id,
                buffer_id: buffer.buffer_id,
            })
            .await?;
        println!(
            "Remote diff baseline bytes: base={:?}, index={:?}; mode={}",
            diff.committed_text.as_ref().map(String::len),
            diff.staged_text.as_ref().map(String::len),
            diff.mode
        );
        ensure!(
            diff.committed_text.is_some(),
            "choose a tracked text file with a committed baseline"
        );
        if let Ok(history_mode) = std::env::var("ZED_VCS_PROBE_HISTORY") {
            let repository_id = *probe
                .repositories
                .keys()
                .next()
                .context("missing repository")?;
            let request_id = probe
                .send(
                    proto::GetInitialGraphData {
                        project_id,
                        repository_id,
                        log_source: Some(proto::GitLogSource {
                            source: Some(proto::git_log_source::Source::All(
                                proto::GitLogSourceAll {},
                            )),
                        }),
                        log_order: proto::get_initial_graph_data::LogOrder::DateOrder as i32,
                    }
                    .into_envelope(0, None, None),
                )
                .await?;
            let mut commits = Vec::new();
            loop {
                let message = probe.receive().await?;
                if message.responding_to != Some(request_id) {
                    continue;
                }
                match message.payload {
                    Some(Payload::GetInitialGraphDataResponse(response)) => {
                        commits.extend(response.commits)
                    }
                    Some(Payload::EndStream(_)) => break,
                    Some(Payload::Error(error)) => bail!("history: {}", error.message),
                    _ => bail!("unexpected history response"),
                }
            }
            let first = match std::env::var("ZED_VCS_PROBE_COMMIT") {
                Ok(revision) => commits
                    .iter()
                    .find(|commit| commit.sha == revision)
                    .context("requested commit is not in history")?,
                Err(_) => commits.first().context("history is empty")?,
            };
            let data = probe
                .request(proto::GetCommitData {
                    project_id,
                    repository_id,
                    shas: vec![first.sha.clone()],
                })
                .await?;
            ensure!(data.commits.len() == 1, "missing history metadata");
            println!(
                "Remote history: {} commits; first message has {} bytes",
                commits.len(),
                data.commits[0].message.len()
            );
            if history_mode == "diff" {
                let diff = probe
                    .request(proto::LoadCommitDiff {
                        project_id,
                        repository_id,
                        commit: first.sha.clone(),
                        ignore_shallow_boundary: false,
                    })
                    .await?;
                ensure!(
                    !diff.files.is_empty(),
                    "choose a history with a nonempty first commit"
                );
                let omitted = diff
                    .files
                    .iter()
                    .filter(|file| file.omitted_reason.is_some())
                    .count();
                println!(
                    "Remote historical diff: {} files, {} omitted",
                    diff.files.len(),
                    omitted
                );
                if let Ok(path) = std::env::var("ZED_VCS_PROBE_EXPECT_OMITTED") {
                    let file = diff
                        .files
                        .iter()
                        .find(|file| file.path == path)
                        .context("missing omitted file")?;
                    ensure!(
                        file.omitted_reason.is_some(),
                        "expected a size-limit omission"
                    );
                    ensure!(
                        !file.is_binary,
                        "oversized text must not be mislabeled as binary"
                    );
                    ensure!(
                        diff.files.iter().any(|file| file.omitted_reason.is_none()
                            && file.new_text.as_ref().is_some_and(|text| !text.is_empty())),
                        "missing ordinary file contents"
                    );
                }
            }
        }
        probe
            .request(proto::RestrictWorktrees {
                project_id,
                worktree_ids: vec![worktree.worktree_id],
            })
            .await?;
        while !probe.repositories.is_empty() {
            probe.receive().await?;
        }
        println!("Trust revocation removed the remote repository");
        // Use a dedicated proxy identifier: this shuts down that test session only.
        if let Err(error) = probe.request(proto::ShutdownRemoteServer {}).await {
            // Server shutdown can close the sockets before its Ack is flushed.
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
            {
                return Err(error);
            }
        }
        drop(probe);
        let status = child.status().await?;
        // The proxy reports a closed server socket as an error even after requested shutdown.
        let server_not_running = remote::proxy::ProxyLaunchError::ServerNotRunning.to_exit_code();
        ensure!(
            status.success()
                || status.code() == Some(1)
                || status.code() == Some(server_not_running),
            "proxy exited with {status}"
        );
        println!("Read-only smoke test passed; test proxy stopped ({status})");
        Ok(())
    })
}
