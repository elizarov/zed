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
