use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use smol::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    lock::Mutex,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Duration,
};

pub const PROTOCOL_VERSION: &str = "0.1";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub read_only: bool,
    pub staging: bool,
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub branches: bool,
    #[serde(default)]
    pub tags: bool,
    #[serde(default)]
    pub tracking: bool,
    #[serde(default)]
    pub blame: bool,
    #[serde(default)]
    pub branch_diff: bool,
    #[serde(default)]
    pub permalinks: bool,
    #[serde(default)]
    pub diff_stats: bool,
    #[serde(default)]
    pub stashes: bool,
    #[serde(default)]
    pub worktrees: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReferenceKind {
    Branch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    pub name: String,
    pub kind: ReferenceKind,
    pub revision: String,
    pub upstream: Option<Tracking>,
    pub commit: Option<ReferenceCommit>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceCommit {
    pub timestamp: i64,
    pub subject: String,
    pub author_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Tracking {
    pub name: String,
    #[serde(default)]
    pub gone: bool,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryInfo {
    pub id: String,
    pub root: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ChangeKind {
    #[default]
    Unchanged,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Ignored,
    Conflicted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    #[serde(default)]
    pub status: ChangeKind,
    #[serde(default)]
    pub staged_status: ChangeKind,
    pub original_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub snapshot: String,
    pub revision: Option<String>,
    pub branch: Option<String>,
    pub changes: Vec<Change>,
    #[serde(default)]
    pub references: Vec<Reference>,
}

impl Snapshot {
    fn validate_capabilities(&self, capabilities: Capabilities) -> Result<()> {
        for reference in &self.references {
            ensure!(
                match reference.kind {
                    ReferenceKind::Tag => capabilities.tags,
                    _ => capabilities.branches,
                },
                "reference requires its advertised capability"
            );
            ensure!(
                reference.upstream.is_none() || capabilities.tracking,
                "upstream requires tracking capability"
            );
        }
        Ok(())
    }
    fn validate(&self) -> Result<()> {
        ensure!(!self.snapshot.is_empty(), "empty snapshot token");
        let mut references = HashSet::new();
        for reference in &self.references {
            validate_reference_name(&reference.name)?;
            validate_revision(&reference.revision)?;
            ensure!(
                references.insert((reference.kind as u8, &reference.name)),
                "duplicate reference"
            );
            if let Some(upstream) = &reference.upstream {
                ensure!(
                    reference.kind == ReferenceKind::Branch,
                    "only local branches track upstreams"
                );
                validate_reference_name(&upstream.name)?;
                ensure!(
                    upstream.ahead.is_some() == upstream.behind.is_some(),
                    "incomplete tracking counts"
                );
                ensure!(
                    !upstream.gone || upstream.ahead.is_none(),
                    "gone upstream has tracking counts"
                );
            }
        }
        let mut paths = HashSet::new();
        for change in &self.changes {
            validate_path(&change.path)?;
            ensure!(paths.insert(&change.path), "duplicate status path");
            if let Some(original_path) = &change.original_path {
                validate_path(original_path)?;
            }
            ensure!(
                !matches!(
                    change.staged_status,
                    ChangeKind::Untracked | ChangeKind::Ignored | ChangeKind::Conflicted
                ),
                "invalid staged status"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub path: String,
    pub base: Option<String>,
    pub index: Option<String>,
}

pub const MAX_HISTORY_COMMITS: usize = 200;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCommit {
    pub id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: i64,
    pub message: String,
}

impl HistoryCommit {
    fn validate(&self) -> Result<()> {
        validate_revision(&self.id)?;
        let mut parents = HashSet::new();
        for parent in &self.parents {
            validate_revision(parent)?;
            ensure!(
                parent != &self.id && parents.insert(parent),
                "invalid commit parents"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub commits: Vec<HistoryCommit>,
    pub has_more: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitChange {
    pub path: String,
    pub base: Option<String>,
    pub target: Option<String>,
}

fn validate_reference_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 4096 && !name.chars().any(char::is_control),
        "invalid reference name"
    );
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    ensure!(
        !revision.is_empty() && revision.len() <= 4096 && !revision.contains('\0'),
        "invalid revision identifier"
    );
    Ok(())
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0', ':']),
        "invalid relative path"
    );
    ensure!(
        path.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "path escapes repository scope"
    );
    Ok(())
}

struct ProcessTransport {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
    failed: bool,
    timeout: Duration,
}

impl ProcessTransport {
    #[allow(
        clippy::disallowed_methods,
        reason = "This standalone protocol crate has no GPUI dependency; GPUI tests use its in-memory transport"
    )]
    async fn request<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T> {
        ensure!(
            !self.failed,
            "VCS provider disconnected; reopen the project to restart it"
        );
        let timeout = self.timeout;
        let result = smol::future::race(self.exchange(method, params), async move {
            smol::Timer::after(timeout).await;
            bail!("VCS provider request timed out")
        })
        .await;
        // A complete JSON-RPC error is recoverable; transport errors are not. A timed-out
        // reader may have consumed part of a frame, so the stream cannot be reused.
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.failed = true;
                if let Err(kill_error) = self.child.kill()
                    && kill_error.kind() != std::io::ErrorKind::InvalidInput
                {
                    return Err(error.context(format!("terminating provider: {kill_error}")));
                }
                self.child.status().await.context("reaping VCS provider")?;
                return Err(error);
            }
        };
        if let Some(error) = value.get("error") {
            return Err(serde_json::from_value::<ProviderError>(error.clone())?.into());
        }
        serde_json::from_value(value.get("result").context("missing result")?.clone())
            .context("invalid VCS provider result")
    }

    async fn exchange(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let body = serde_json::to_vec(
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )?;
        ensure!(body.len() <= MAX_MESSAGE_BYTES, "request too large");
        self.input
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        self.input.write_all(&body).await?;
        self.input.flush().await?;
        loop {
            let value = read_message(&mut self.output).await?;
            ensure!(
                value.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
                "invalid JSON-RPC version"
            );
            if value.get("method").and_then(Value::as_str) == Some("repository/changed")
                && value.get("id").is_none()
            {
                // This prototype polls snapshots. Notifications may be coalesced until
                // the next poll; providers must never depend on an acknowledgement.
                continue;
            }
            ensure!(
                value.get("id").and_then(Value::as_u64) == Some(id),
                "unexpected response id"
            );
            ensure!(
                value.get("result").is_some() != value.get("error").is_some(),
                "expected exactly one result or error"
            );
            return Ok(value);
        }
    }
}

pub async fn read_message(reader: &mut (impl smol::io::AsyncBufRead + Unpin)) -> Result<Value> {
    let mut header_bytes = 0;
    let mut length = None;
    loop {
        // Limit each read as well as the total header, including an unterminated line.
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take(8193)
            .read_until(b'\n', &mut line)
            .await?;
        ensure!(read > 0, "VCS provider closed stdout");
        header_bytes += read;
        ensure!(header_bytes <= 8192, "VCS provider header too large");
        if line == b"\r\n" {
            break;
        }
        let line = std::str::from_utf8(&line)?.trim_end_matches("\r\n");
        let (name, value) = line.split_once(':').context("invalid frame header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            ensure!(length.is_none(), "duplicate Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing Content-Length")?;
    ensure!(
        length <= MAX_MESSAGE_BYTES,
        "VCS provider message too large"
    );
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Debug, Deserialize)]
struct ProviderError {
    code: i64,
    message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "VCS provider error {}: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for ProviderError {}

pub fn is_size_limit_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ProviderError>()
        .is_some_and(|error| error.code == -32002)
}

#[cfg(feature = "test-support")]
type MockRequestHandler = dyn FnMut(&str, Value) -> Result<Value> + Send;

enum Connection {
    Process(ProcessTransport),
    #[cfg(feature = "test-support")]
    Mock(Box<MockRequestHandler>),
}

impl Connection {
    async fn request<T: DeserializeOwned>(&mut self, method: &str, parameters: Value) -> Result<T> {
        match self {
            Self::Process(transport) => transport.request(method, parameters).await,
            #[cfg(feature = "test-support")]
            Self::Mock(handler) => Ok(serde_json::from_value(handler(method, parameters)?)?),
        }
    }
}

pub struct Client {
    transport: std::sync::Arc<Mutex<Connection>>,
    pub repository: RepositoryInfo,
    pub capabilities: Capabilities,
    snapshot: RwLock<Snapshot>,
}

impl Client {
    pub async fn start(
        command: &str,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
        root: &Path,
        timeout: Duration,
    ) -> Result<Self> {
        let mut child = Command::new(command)
            .args(arguments)
            .envs(environment)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("starting VCS provider")?;
        let mut transport = ProcessTransport {
            input: child.stdin.take().context("missing provider stdin")?,
            output: BufReader::new(child.stdout.take().context("missing provider stdout")?),
            child,
            next_id: 0,
            failed: false,
            timeout,
        };
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Initialization {
            protocol_version: String,
            capabilities: Capabilities,
        }
        let initialized: Initialization = transport
            .request(
                "initialize",
                json!({"protocolVersion":PROTOCOL_VERSION, "client":{"name":"Zed"}}),
            )
            .await?;
        ensure!(
            initialized.protocol_version == PROTOCOL_VERSION,
            "unsupported VCS protocol version"
        );
        ensure!(
            initialized.capabilities.read_only,
            "provider must support read-only operation"
        );
        let capabilities = initialized.capabilities;
        ensure!(
            !capabilities.tracking || capabilities.branches,
            "tracking requires branches capability"
        );
        ensure!(
            !(capabilities.blame
                || capabilities.branch_diff
                || capabilities.permalinks
                || capabilities.diff_stats
                || capabilities.stashes
                || capabilities.worktrees),
            "provider advertises features not implemented by protocol 0.1"
        );
        let repository: Option<RepositoryInfo> = transport
            .request("repository/discover", json!({"workspaceRoot":root}))
            .await?;
        let repository = repository.context("provider did not recognize this workspace")?;
        ensure!(
            Path::new(&repository.root) == root,
            "provider root must equal the requested workspace scope"
        );
        ensure!(
            !repository.id.is_empty() && !repository.label.is_empty(),
            "empty repository identity"
        );
        let snapshot: Snapshot = transport
            .request("repository/status", json!({"repository":repository.id}))
            .await?;
        snapshot.validate()?;
        snapshot.validate_capabilities(capabilities)?;
        Ok(Self {
            transport: std::sync::Arc::new(Mutex::new(Connection::Process(transport))),
            repository,
            capabilities: initialized.capabilities,
            snapshot: RwLock::new(snapshot),
        })
    }

    #[cfg(feature = "test-support")]
    pub fn mock(
        repository: RepositoryInfo,
        snapshot: Snapshot,
        handler: impl FnMut(&str, Value) -> Result<Value> + Send + 'static,
    ) -> Result<Self> {
        snapshot.validate()?;
        Ok(Self {
            repository,
            capabilities: Capabilities {
                read_only: true,
                staging: true,
                ..Default::default()
            },
            snapshot: RwLock::new(snapshot),
            transport: std::sync::Arc::new(Mutex::new(Connection::Mock(Box::new(handler)))),
        })
    }

    pub fn stop(&self) {
        let transport = self.transport.clone();
        smol::spawn(async move {
            let mut transport = transport.lock().await;
            #[allow(irrefutable_let_patterns)]
            let Connection::Process(transport) = &mut *transport else {
                return;
            };
            transport.failed = true;
            // Dropping the child also kills it, but trust revocation can leave UI
            // references alive. Close the process independently of those references.
            if let Err(error) = transport.child.kill()
                && error.kind() != std::io::ErrorKind::InvalidInput
            {
                eprintln!("terminating VCS provider: {error}");
            }
            if let Err(error) = transport.child.status().await {
                eprintln!("reaping VCS provider: {error}");
            }
        })
        .detach();
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.read().clone()
    }

    pub async fn refresh(&self) -> Result<()> {
        let mut transport = self.transport.lock().await;
        let snapshot: Snapshot = transport
            .request(
                "repository/status",
                json!({"repository":self.repository.id}),
            )
            .await?;
        snapshot.validate()?;
        snapshot.validate_capabilities(self.capabilities)?;
        *self.snapshot.write() = snapshot;
        Ok(())
    }

    pub async fn history(
        &self,
        revision: &str,
        path: Option<&str>,
        limit: usize,
    ) -> Result<History> {
        ensure!(
            self.capabilities.history,
            "VCS provider does not support history"
        );
        validate_revision(revision)?;
        ensure!(
            (1..=MAX_HISTORY_COMMITS).contains(&limit),
            "invalid history limit"
        );
        if let Some(path) = path {
            validate_path(path)?;
        }
        let history: History = self.transport.lock().await.request(
            "repository/history",
            json!({"repository":self.repository.id, "revision":revision, "path":path, "limit":limit}),
        ).await?;
        ensure!(
            history.commits.len() <= limit,
            "provider exceeded history limit"
        );
        let mut ids = HashSet::new();
        for commit in &history.commits {
            commit.validate()?;
            ensure!(ids.insert(&commit.id), "duplicate history commit");
        }
        Ok(history)
    }

    pub async fn commit_details(&self, revision: &str) -> Result<HistoryCommit> {
        ensure!(
            self.capabilities.history,
            "VCS provider does not support history"
        );
        validate_revision(revision)?;
        let commit: HistoryCommit = self
            .transport
            .lock()
            .await
            .request(
                "repository/commitDetails",
                json!({"repository":self.repository.id, "revision":revision}),
            )
            .await?;
        commit.validate()?;
        ensure!(commit.id == revision, "wrong commit identifier");
        Ok(commit)
    }

    pub async fn commit_changes(&self, revision: &str) -> Result<Vec<CommitChange>> {
        ensure!(
            self.capabilities.history,
            "VCS provider does not support history"
        );
        validate_revision(revision)?;
        let changes: Vec<CommitChange> = self
            .transport
            .lock()
            .await
            .request(
                "repository/commitChanges",
                json!({"repository":self.repository.id, "revision":revision}),
            )
            .await?;
        ensure!(changes.len() <= 4096, "commit exceeds the file limit");
        let mut paths = HashSet::new();
        for change in &changes {
            validate_path(&change.path)?;
            ensure!(paths.insert(&change.path), "duplicate commit path");
            ensure!(
                change.base.is_some() || change.target.is_some(),
                "empty commit change"
            );
        }
        Ok(changes)
    }

    pub async fn read_content(&self, reference: &str) -> Result<Vec<u8>> {
        #[derive(Deserialize)]
        struct Content {
            encoding: String,
            data: String,
        }
        let content: Content = self
            .transport
            .lock()
            .await
            .request(
                "repository/readContent",
                json!({"repository":self.repository.id, "content":reference}),
            )
            .await?;
        ensure!(content.encoding == "base64", "unsupported content encoding");
        STANDARD
            .decode(content.data)
            .context("invalid base64 content")
    }

    pub async fn contents(
        &self,
        paths: &[String],
        staged: &[bool],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let result = self.read_contents(paths, staged).await;
        if result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ProviderError>())
            .is_some_and(|error| error.code == -32001)
        {
            self.refresh().await?;
            return self.read_contents(paths, staged).await;
        }
        result
    }

    async fn read_contents(
        &self,
        paths: &[String],
        staged: &[bool],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        ensure!(
            paths.len() == staged.len() && paths.len() <= 4096,
            "invalid comparison batch size"
        );
        for path in paths {
            validate_path(path)?;
        }
        let mut transport = self.transport.lock().await;
        let snapshot = self.snapshot.read().snapshot.clone();
        let comparisons: Vec<Comparison> = transport
            .request(
                "repository/comparison",
                json!({"repository":self.repository.id, "snapshot":snapshot, "paths":paths}),
            )
            .await?;
        ensure!(comparisons.len() == paths.len(), "wrong comparison count");
        let mut result = Vec::with_capacity(paths.len());
        for ((comparison, path), staged) in comparisons.into_iter().zip(paths).zip(staged) {
            ensure!(comparison.path == *path, "wrong comparison path");
            let reference = if *staged {
                comparison.index
            } else {
                comparison.base
            };
            result.push(if let Some(reference) = reference {
                #[derive(Deserialize)]
                struct Content {
                    encoding: String,
                    data: String,
                }
                let content: Content = transport
                    .request(
                        "repository/readContent",
                        json!({"repository":self.repository.id, "content":reference}),
                    )
                    .await?;
                ensure!(content.encoding == "base64", "unsupported content encoding");
                Some(
                    STANDARD
                        .decode(content.data)
                        .context("invalid base64 content")?,
                )
            } else {
                None
            });
        }
        Ok(result)
    }
}
