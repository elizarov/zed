# External VCS Provider Protocol {#external-vcs-provider-protocol}

This fork implements an **experimental, read-only protocol, version `0.1`** for
external version control systems. It is not an upstream Zed extension API.
Providers are separate executable processes. They supply repository status and
immutable file baselines; Zed owns file editing, tree decorations, gutters, and
diff rendering. No Git repository or Git metadata is required.

The public boundary is the JSON protocol described here, implemented by
`crates/vcs_provider`. `crates/git/src/repository/external.rs` adapts it to Zed's
current repository interface; `crates/project/src/git_store/external.rs` handles
project registration and refresh. A provider must not depend on those internal
Rust interfaces. Settings registration is the prototype's bootstrap mechanism;
extension manifests or a WASM registration API can later launch the same protocol.

## Registering a Provider {#registering-a-provider}

Put this top-level setting in a trusted project's `.zed/settings.json`:

```json
{
  "vcs_provider": {
    "command": "/absolute/path/to/example-vcs-provider",
    "args": ["--stdio"],
    "env": {},
    "poll_interval_ms": 2000,
    "request_timeout_ms": 30000
  }
}
```

`command` is required; `args` and `env` default to empty collections. The process
inherits Zed's environment with `env` overrides, runs in the opened project root,
and is launched directly, without shell interpretation. Use absolute executable
paths; shell expansions, including `~`, do not apply. Polling defaults to 2000 ms
(minimum 250); request timeouts default to 30000 ms (clamped to 100–300000).

The setting selects one external provider per opened local worktree, including
when the worktree is a subdirectory of a larger repository. It supersedes native
Git discovery in that worktree. Restart the project after configuration changes
or a failed process launch. Startup errors are recorded in Zed's log.

An untrusted worktree cannot launch a provider. Restricting or closing a worktree
stops its provider. This prototype does not transport external repositories to
collaboration guests or SSH clients; configure it only in local projects.

## Transport and Lifecycle {#transport-and-lifecycle}

The host starts one process per workspace scope. Requests and responses use
JSON-RPC 2.0, encoded as UTF-8 JSON on stdin/stdout, with LSP-style framing:

```text
Content-Length: <UTF-8 byte count>\r\n
\r\n
<JSON body>
```

Here `\r\n` denotes CRLF bytes, not literal backslashes. The count includes only
the body. Headers end with a blank CRLF line. Header names are case insensitive;
one `Content-Length` is required. The host permits at most 8192 header bytes and
32 MiB per JSON body. Providers must reject oversized input before allocating the
body. Diagnostic messages go to stderr, never stdout.

Requests use monotonically increasing positive integer `id` values. A response
must echo the ID and contain exactly one of `result` or `error`. Example bodies:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": { "protocolVersion": "0.1", "client": { "name": "Zed" } }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "0.1",
    "capabilities": { "readOnly": true, "staging": true }
  }
}
```

The sequence is `initialize`, `repository/discover`, then status/comparison/content
requests. Version negotiation is exact in this prototype: incompatible versions
fail initialization. `readOnly: true` is mandatory and means the session exposes
no VCS mutations. `staging` advertises a separate staging baseline, **not** permission
to stage. It must be present and boolean.

Zed serializes requests per process. A complete JSON-RPC error leaves the connection
usable. EOF, a malformed frame, an unexpected response ID, or a request timeout
terminates the connection. Timeouts kill and reap the process because a partially
read frame cannot safely be resumed. There is no automatic process restart or
per-request cancellation in version 0.1. The host closes/kills the child when its
scope ends; providers should exit cleanly on stdin EOF. There is no shutdown RPC.

## Repository Discovery {#repository-discovery}

`repository/discover` takes:

```json
{ "workspaceRoot": "/work/large-repository/service" }
```

It returns `null` if unsupported, or:

```json
{
  "id": "service",
  "root": "/work/large-repository/service",
  "label": "Example VCS"
}
```

`id` is a nonempty opaque process-local identifier. `label` is nonempty human
readable text. `root` must equal the requested absolute workspace path: the
provider may internally discover a containing repository, but must scope paths,
status, and comparisons to the opened directory. Version 0.1 supports one discovery
and one repository per process. A provider returning `null` is not registered.

All subsequent methods take `repository: id`. Unknown IDs are invalid parameters.
All file paths use `/` separators and are relative to `root`. Empty paths, empty
components, `.`, `..`, backslashes, colons, and NUL are invalid. Paths are case
sensitive protocol strings; providers preserve their actual spelling. Unicode
and spaces are supported. Paths outside the workspace must never be published.

## Status Snapshots {#status-snapshots}

`repository/status` takes `{ "repository": "service" }` and returns a **complete**
scoped status snapshot:

```json
{
  "snapshot": "state-42",
  "revision": "opaque-current-revision",
  "branch": "main",
  "changes": [
    {
      "path": "src/example.txt",
      "status": "modified",
      "stagedStatus": "modified",
      "originalPath": null
    },
    { "path": "new.txt", "status": "untracked" },
    { "path": "removed.txt", "status": "deleted" }
  ]
}
```

`snapshot` is a nonempty opaque token. It identifies coherent repository metadata
and immutable base/staging content, not a frozen working directory. Reuse it only
while that metadata and those baselines remain identical. Change it when the
revision, branch, status list, or staged content changes, including restaging a
file whose status label remains unchanged. Working file bytes come from Zed's live
buffers/filesystem and are refreshed through its normal file watching.

`revision` and `branch` are nullable display strings; a revision need not be a Git
SHA. Each path appears at most once. `status` describes working-copy changes from
the staging baseline; `stagedStatus` describes staging changes from the base.
Both default to `unchanged` when omitted. Providers without staging use
`stagedStatus: "unchanged"` and compare working files directly with the base.

Allowed status values:

| Value         | Meaning                              |
| ------------- | ------------------------------------ |
| `unchanged`   | No change on this side               |
| `modified`    | Modified file                        |
| `added`       | Added file                           |
| `deleted`     | Deleted file                         |
| `renamed`     | Renamed file                         |
| `copied`      | Copied file                          |
| `typeChanged` | File kind or mode changed            |
| `untracked`   | Working file outside version control |
| `ignored`     | Ignored working file                 |
| `conflicted`  | Unresolved conflict                  |

`untracked`, `ignored`, and `conflicted` are only valid in `status`. Directory
summaries are computed by Zed; publish file entries, not duplicate directory
entries. Clean files can be omitted but must still support comparisons. A staged
deletion followed by a newly created working file is `stagedStatus: "deleted"`,
`status: "added"`.

`originalPath` is an optional scoped source path for a rename/copy. The provider
supplies the appropriate original content through the comparison method. The
prototype displays rename, copy, and type changes as modifications at the
destination so they participate in native tree summaries and diffs. It does not
render a separate old-name column. Moves across the workspace boundary are represented as an
addition or deletion within the workspace, not a path outside it.

A status error must not be turned into an empty success. Zed retains its last
valid snapshot, reports refresh failures in the log and repository error state,
and retries at the next poll. Snapshot updates should be inexpensive: listing
status must not materialize the content of every tracked file.

## Comparisons and Content {#comparisons-and-content}

`repository/comparison` takes a snapshot token and up to 4096 requested paths:

```json
{
  "repository": "service",
  "snapshot": "state-42",
  "paths": ["src/example.txt", "new.txt"]
}
```

It returns an array with exactly one entry per requested path, in request order,
including duplicate paths if requested:

```json
[
  { "path": "src/example.txt", "base": "blob-A", "index": "blob-B" },
  { "path": "new.txt", "base": null, "index": null }
]
```

`base` is the committed/baseline content and `index` is the staging baseline.
These are immutable, opaque content references, not executable commands or file
URLs. Providers without staging return the same reference in both fields. `null`
means that file does not exist on that side (also used for symlinks, for which the
prototype does not show text baselines). An empty file uses a non-null reference
that resolves to zero bytes. A deleted working file still has a baseline.

The method must support clean files, so editor gutters work before a file appears
in the status list. Resolve every requested side against the specified snapshot,
not whatever is currently checked out. Renames/copies may use the original path's
content as the destination's base. Working endpoints are always Zed's actual file
or buffer; a provider does not send or replace working content.

`repository/readContent` takes:

```json
{ "repository": "service", "content": "blob-A" }
```

It returns:

```json
{ "encoding": "base64", "data": "b2xkIGNvbnRlbnQK" }
```

`encoding` must be `base64`; `data` is standard padded base64 of the exact bytes.
Do not normalize newlines or decode/re-encode text. The JSON body limit includes
the base64 expansion (approximately 24 MiB maximum raw content). Oversized files
return `-32002`; streaming content and binary diff rendering are out of scope.
Zed applies its existing text decoding and binary-file handling.

References must remain immutable. Providers may expire old snapshots/references,
but must return `-32001`, never silently resolve them against newer state. Keep at
least the latest snapshot valid until a newer status response has been returned.
Zed refreshes and retries a comparison/content batch once on `-32001`.

## Refresh Notifications {#refresh-notifications}

A provider may send this notification between any response frames:

```json
{
  "jsonrpc": "2.0",
  "method": "repository/changed",
  "params": { "repository": "service" }
}
```

It is an invalidation hint, not a partial status update. The prototype reads and
coalesces these hints during the next request and refreshes by polling; sending a
notification does not currently wake an idle host sooner. Providers may omit
notifications entirely. A successful changed snapshot refreshes tree status and
open diff baselines; unchanged tokens avoid unnecessary diff reloads.

## Errors and Read-only Contract {#errors-and-read-only-contract}

Errors use normal JSON-RPC error objects, for example:

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "error": { "code": -32001, "message": "Snapshot expired; refresh status" }
}
```

| Code     | Meaning                                   |
| -------- | ----------------------------------------- |
| `-32700` | Parse/framing failure                     |
| `-32600` | Invalid request                           |
| `-32601` | Unknown or unsupported method             |
| `-32602` | Invalid parameters, path, or repository   |
| `-32000` | Provider/backend failure                  |
| `-32001` | Snapshot or content reference expired     |
| `-32002` | Content/result exceeds supported limits   |
| `-32003` | Concurrent repository change; retry later |
| `-32010` | Unsupported protocol version              |
| `-32011` | Session has not been initialized          |

The host exposes status and native diffs, including optional staged/unstaged
views. It disables commit, stage/unstage, restore, and related VCS controls,
including individual hunk operations, and rejects backend mutations. Ordinary
file editing remains available. Providers must implement only read operations;
there is no generic command execution or write-content endpoint.

History, blame, branch switching, conflict resolution, Git diff statistics,
network operations, arbitrary resource groups, and remote/collaboration transport
are not implemented. Some existing UI labels still say "Git". This adapter is a
path toward a generic repository model rather than a complete replacement of
Zed's Git-specific UI.

## Validation {#validation}

From the repository root:

```sh
cargo test -p vcs_provider
cargo test -p project external_provider_
cargo check -p git_ui
```

The protocol suite uses a real mock process and covers framing, initialization,
status, notifications, lazy binary content, expired snapshots, provider errors,
and timeouts. The
project tests use an in-memory mock for native tree status, live baseline refresh,
and blocked writes, plus a mock process for settings, trust revocation, and restart behavior.

To inspect a provider through the same Rust client used by Zed:

```sh
cargo run -p vcs_provider --example inspect -- \
  /work/large-repository/service src/example.txt \
  /absolute/path/to/example-vcs-provider --stdio
```

The inspector reports timings and baseline sizes without printing file contents.
