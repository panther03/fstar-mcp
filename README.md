# F* MCP Server

An MCP server over F*'s `--ide` protocol. It keeps a verifying process and a
lax companion process warm for each active file, so agents can edit files on
disk and request incremental checks without resending the source.

## Install and run

```bash
cargo build --release
./target/release/fstar-mcp
```

The transport is stdio, which gives each MCP client its own process and session
namespace. All logging goes to stderr so it can never corrupt the JSON-RPC
stream on stdout. Add `--verbose` to log F* protocol traffic to stderr.

Register it with a client, for example the Copilot CLI:

```bash
copilot mcp add fstar --timeout 600000 -- /path/to/fstar-mcp
```

Clients that speak MCP revision `2026-07-28` open the lifecycle with a
`server/discover` request. This server answers unsupported requests with
`-32601 Method not found`, so such clients fall back to the `initialize`
handshake instead of dropping the connection.

## Recommended workflow

1. Edit an `.fst` or `.fsti` file using the host's normal editing tools.
2. Call `typecheck_buffer` with `file_path`. The server reads the file from
   disk, discovers its project configuration, and creates or reuses a warm
   session.
3. Make targeted edits below the verified prefix and check again. The response
   reports fragment reuse, the last verified line, duration, content hash, and
   staleness.
4. Use `lookup_symbol` for fast type and definition queries through the lax
   companion. Use `get_proof_context` for proof states and `get_status` only
   when detailed fragment ranges are needed.

`content` is an optional unsaved-buffer override for `typecheck_buffer`; disk
is the default source of truth.

## Configuration discovery

For each file, the server uses the first available source:

1. The nearest `*.fst.config.json`, walking toward `workspace_root` when one is
   provided. `$VAR` and `${VAR}` are expanded in all string values.
2. `make <File.fst>-in` in the file's directory. `--include` pairs are split
   from the remaining F* options.
3. Bare defaults using `fstar.exe` from `PATH`.

Relative executable paths are resolved from the configured `cwd`; executable
lookup errors identify both the command and working directory. If a discovered
config names an `fstar_exe` that does not exist — common for checked-in configs
that point at in-tree compiler builds — the server falls back to `fstar.exe`
from `PATH` and logs a warning. An `fstar_exe` passed explicitly to
`create_session` is never overridden this way. Explicit arguments to
`create_session` override discovered values.

Example:

```json
{
  "fstar_exe": "bin/fstar.exe",
  "options": ["--z3rlimit_factor", "2"],
  "include_dirs": ["lib", "${HOME}/fstar/ulib"]
}
```

## Tools

| Tool | Purpose |
|---|---|
| `typecheck_buffer` | Read and check a file, with optional lax/position mode and deadline. Sessions are implicit by client, path, workspace, and configuration. |
| `check_project` | Check selected files, or all F* files below a workspace, in source dependency order. |
| `lookup_symbol` | Query type, docs, and definition through the lax companion. |
| `get_proof_context` | Return proof states from the latest check. |
| `get_status` | Return detailed fragment ranges from the latest check. |
| `restart_solver` | Terminate wedged Z3 descendants before restarting both solvers. |
| `create_session` | Explicitly warm a session without performing an initial full verification. |
| `update_buffer` | Add an unsaved dependency to both F* virtual file systems. |
| `list_sessions` | List only sessions owned by the current MCP client. |
| `close_session` | Close a session owned by the current MCP client. |

Checks have a 60-second default deadline and return partial progress on expiry.
When source dependencies change, the next full check uses `reload-deps` and
lists the changed files. The server also invokes `fstar.exe --dep full` once
per session when available and supplements it with direct `open`/`include`
discovery.

Diagnostics are capped at 20 per file in normal checks and retain F* error
numbers plus related ranges. Full fragment arrays are available separately via
`get_status`.

## Resource and lifecycle settings

| Variable | Default | Meaning |
|---|---:|---|
| `FSTAR_MCP_MAX_SESSIONS` | `4` | Maximum concurrent full/lax process pairs; least-recently-used idle sessions are evicted. |
| `FSTAR_MCP_IDLE_TIMEOUT` | `1800` | Idle seconds before an unowned session is swept. |
| `FSTAR_MCP_SWEEP_PERIOD` | `300` | Seconds between cleanup sweeps. |
Each stdio server instance namespaces sessions by canonical path and effective
configuration. Explicit session IDs are unguessable capabilities, and every
session-taking tool verifies them before use.

## Development

```bash
cargo test
```

The protocol tests use an executable fake F* process to exercise streamed
query IDs, response buffering, cancellation, and partial timeouts. A real-F*
smoke test is included but ignored by default:

```bash
cargo test --test fstar_process_tests real_fstar_typechecks_a_fixture -- --ignored
```
