# `FStarLang/fstar-mcp` — gap analysis

Review of `FStarLang/fstar-mcp` @ `6d1e98a` ("Add a timeout option to create_session"),
~2.2k LOC Rust, 8 MCP tools. Read in full on 2026-07-28/29.

**Verdict:** a working prototype that proves the concept — it does hold a warm
`fstar.exe --ide` process and does drive `full-buffer` queries, which is the whole point.
But in its current state it will not deliver the interactive win on a real F* project, for
four independent reasons: it can't discover project configuration, it makes the *model*
resend the entire file on every check, it can't cancel or pre-empt an in-flight proof, and
it has no notion of multi-file dependencies. There are also two outright correctness bugs.

Everything below is grouped by severity, with the file/line evidence and, where this repo
(`fstar-vscode-assistant`) already solves the same problem, a pointer to the working
implementation.

Items are grouped by severity; the numbers are stable identifiers in discovery order, so
they are not sequential within a group.

---

## P0 — Blockers: without these it does not work on a real project

### 1. No project configuration discovery

`FStarConfig` is a plain struct with `include_dirs`, `options`, `fstar_exe`, `cwd`, all
supplied by the caller (`src/fstar/config.rs`). There is no logic to *find* a project's
configuration. `create_session` just forwards whatever the model passed
(`src/mcp/tools.rs:82-93`), defaulting `cwd` to the file's parent directory and everything
else to empty.

Consequence: on any project that isn't a single self-contained `.fst` file, F* starts with
no `--include` paths and no options, so it fails to resolve the first `open` — or, worse,
silently verifies under different options than the project's build uses (e.g. missing
`--z3rlimit_factor`, `--ext`, `--load` of a tactic plugin), producing results that don't
reproduce under `make`.

Requiring the agent to supply include paths is not a fix: the model would have to reverse
engineer the build system, and it would get it wrong.

**Needed:** replicate the discovery chain that editors already use —
1. nearest `*.fst.config.json` walking up from the file to the workspace root, with
   `$VAR`/`${VAR}` environment substitution;
2. fall back to `make <File.fst>-in` and parse the emitted command line into
   options vs `--include` pairs;
3. fall back to a bare default.

Reference implementation: `lspserver/src/fstar.ts` — `findConfigFile`,
`parseConfigFile` (incl. `substituteEnvVars`), `getConfigFromMakefile`, `getFStarConfig`,
`resolveFStarExe` (resolves `fstar.exe` through `which`/relative-to-cwd, which fstar-mcp
also lacks — it passes the string straight to `Command::new`, so a bad path surfaces as a
generic spawn error).

### 2. `typecheck_buffer` requires the model to resend the whole file every cycle

`TypecheckBufferArgs.code: String` is mandatory (`src/mcp/tools.rs:196-205, 292`) and is
passed straight through to the full-buffer query (`tools.rs:228`). Only `create_session`
reads from disk (`tools.rs:98`).

This defeats the purpose. The point of interactive mode is to make the edit cycle cheap;
here every cycle costs O(file) *output* tokens from the model, which for a 1500-line F*
module can easily exceed the time and cost of the verification being saved — and it grows
with file size exactly when incrementality matters most. It also invites a subtle failure:
the model regenerating the file from memory introduces drift between what F* checked and
what is on disk, so a "verified" result may not correspond to the actual file.

**Needed:** make **disk the source of truth**. Tools take a *path*; the server reads and
hashes the file itself. The workflow becomes: agent edits the file with its normal editing
tool → calls check with a path → server reads, checks, reports. Keep an optional `content`
override for unsaved-buffer/scratch use, but it must not be the documented path.

This is the single highest-leverage change in this document.

### 3. No `cancel` support

`grep -rn cancel src/` returns nothing. The IDE protocol's `cancel` query (with
`cancel-line`/`cancel-column`) is never sent.

Consequence: when the agent edits and re-checks while a previous check is still running,
the new query queues behind the old one. If the in-flight fragment is a 60-second Z3 call
on code the agent has already replaced, the agent waits for a result it will throw away.
Worse, in this codebase the wait is unbounded — see #7.

**Needed:** track the last text sent to F*, diff the new text against it, and send `cancel`
at the first differing position before issuing the new full-buffer query.

Reference: `lspserver/src/fstar_connection.ts` → `cancelFBQ`, driven from
`documentState.ts` → `DocumentProcess.changeDoc` (which computes `findFirstDiffPos` and
also walks back `startedProcessingToPosition`).

### 4. No multi-file / dependency awareness

Sessions are per-file and completely independent (`src/session/mod.rs`: `SessionManager`
keyed by path). There is no dependency graph (`fstar.exe --dep`), no `reload-deps` trigger
on dependency change (the `kind` is accepted as a passthrough string but nothing ever
decides to use it), no file watching, and no handling of `.checked` files.

Consequence: the agent edits `A.fst`, then checks `B.fst` which `open`s `A` — the warm `B`
session is still holding the *old* elaborated `A`, so it reports success against stale
dependencies. This is a silent-wrong-answer failure mode, the worst kind for a verification
tool, and it is the normal case in any real proof development.

**Needed:** compute the dependency graph, watch dependency mtimes/hashes, and on change
either send `reload-deps` or restart the affected sessions — and tell the agent explicitly
that `A.fst` must be re-checked/`.checked`-regenerated before `B.fst`'s result is
meaningful. A dependency-ordered `check_project` (falling back to batch/`make` for
cross-module work) is the natural companion.

---

## P1 — Correctness bugs

### 5. Query-id prefix matching is wrong, and drops interleaved responses

In `full_buffer_query` (`src/fstar/process.rs:269-322`) responses are matched with
`query_id.starts_with(&base_qid)`.

Two distinct bugs:

- **False positives.** With `base_qid = "1"`, the ids `"10"`, `"11"`, `"12"`, … all match.
  F* uses a monotonic counter (`next_query_id`, `process.rs`), so this triggers as soon as
  a session exceeds 10 queries — a few minutes into any real agent run. Responses belonging
  to *other* queries get folded into the current full-buffer result, corrupting diagnostics
  and fragment lists.

  F*'s actual convention is that full-buffer sub-responses carry ids of the form
  `<qid>.<n>` (e.g. `2`, `2.1`, `2.2`). The correct demux is to truncate at the first `.`
  and compare for equality — see `fstar_connection.ts` → `removeDotAndRest`.

- **Dropped responses.** Non-matching messages hit `continue` and are discarded from the
  channel. Since the loop owns `&mut self` and the single `response_rx` for the entire
  duration of the full-buffer query, any response to a query issued *before* the
  full-buffer query is silently thrown away, and its caller — which is `await`ing on the
  same receiver — can never observe it. `vfs_add` and `lookup` (`process.rs`) both loop
  `while let Some(response) = self.response_rx.recv().await` with no timeout, so this is an
  unbounded hang.

  The TS server avoids this entirely by keeping a `pending_responses` map keyed by qid and
  by *buffering* non-full-buffer requests until the in-flight full-buffer query finishes
  (`fstar_connection.ts`: `sendReq`, `handleFBQResponse`, `pending_responses`).

### 6. `restart_solver` doesn't actually kill a wedged Z3

`FStarProcess::restart_solver` only sends the `restart-solver` query and returns
(`src/fstar/process.rs`). That is an in-band request: if F* is blocked waiting on a Z3
child that is spinning, the message isn't processed and nothing is recovered.

**Needed:** also enumerate and kill the `z3` descendants of the F* process, then let F*
respawn them — `lspserver/src/fstar.ts` → `killZ3SubProcess` (via `ps-tree`), used by
`FStarConnection.restartSolver` which kills first, waits, then sends the request.

### 19. Multiple clients share global state, with no isolation and no ownership checks

The server is explicitly built for concurrent clients: it is a streamable-HTTP server, and
it tracks which MCP client owns which F* sessions (`Session.mcp_session_id`,
`SessionManager.mcp_to_fstar_sessions`) with an `on_session_closed` hook that marks a
departing client's sessions for deletion (`src/main.rs`). But the isolation is not actually
enforced anywhere:

- **All state is one process-wide singleton.** `SESSION_MANAGER` is a `lazy_static`
  (`src/mcp/tools.rs:19-21`, re-exported via `src/mcp/mod.rs`), so every client shares one
  `sessions` map and one `file_to_session` map.

- **Clients silently evict each other.** `SessionManager::create_session`
  (`src/session/mod.rs`) looks the file path up in the *global* `file_to_session` map and
  unconditionally closes whatever session it finds before creating the new one — it never
  consults `mcp_session_id`. Two agents (or an agent and a human's tooling) working on the
  same file will repeatedly kill each other's warm process, each silently losing its
  verified prefix and paying full re-elaboration cost. This is the worst failure mode here,
  because it looks exactly like "interactive mode just isn't very fast".

- **Ownership is recorded but never checked.** `mcp_session_id` is only used for the
  cleanup sweep. Every tool handler (`typecheck_buffer`, `get_proof_context`,
  `close_session`, …) resolves the caller-supplied `session_id` directly against the global
  map (`src/mcp/tools.rs:226, 318, 390, 500, 649`) with no verification that the caller owns
  it. Any client holding — or guessing — another client's UUID can drive or close its
  session.

- **Clients serialize against one another.** Per #8, each tool call holds
  `SESSION_MANAGER.sessions.write()` for the full duration of its F* query, so one client's
  60-second proof blocks *every* operation of *every* other client, across unrelated files.

- **No workspace scoping.** Sessions live in one flat namespace with no notion of which
  project/workspace they belong to, so clients in different projects (with different
  include paths and options) collide in the same map — compounding #1.

- **Cleanup depends on an unverified assumption.** The whole reclaim path keys off
  `extra.session_id` being populated. `main.rs` constructs `StreamableHttpServerConfig` with
  `session_id_generator: None`; **unverified** whether that yields stateless operation (in
  which case `extra.session_id` is `None`, `mcp_to_fstar_sessions` stays empty, and the
  sweeper never reclaims anything — F* processes then accumulate until the optional
  per-session `timeout` fires, if the client passed one) or falls back to a default UUID
  generator. Worth confirming against the pmcp SDK before anyone depends on multi-client
  use.

**Needed:** scope sessions per client *and* per workspace, not just per file path; enforce
ownership on every session-taking tool; make path-keyed reuse cooperative rather than
destructive (share a warm process between clients, or namespace it by owner) — note that
implicit path-keyed sessions (#14) makes this decision unavoidable and should be designed
with it in mind; and replace the global lock with per-session locks (#8).

---

## P2 — Missing capabilities that make it slow or unusable in an agent loop

### 7. No timeouts on any query except startup

The only `tokio::time::timeout` in the codebase is the 30s wait for `protocol-info`
(`process.rs:196`). `full_buffer_query`, `vfs_add` and `lookup` all `recv().await` in
unbounded loops.

Consequence: a proof that diverges (or a Z3 call with a huge rlimit, or the dropped-response
bug above) hangs the MCP tool call indefinitely. The agent has no way to interrupt it and
the harness eventually kills the whole conversation.

**Needed:** a deadline on every tool call, and — importantly — a **partial** result on
expiry ("verified through line N; fragment at line M still running") rather than an error.
Partial progress is genuinely useful to the agent; a timeout error is not.

### 8. Every tool call holds a global write lock for the duration of the F* query

Each handler takes `SESSION_MANAGER.sessions.write().await` and holds it across the entire
`await` on F* (`src/mcp/tools.rs:104, 226, 318, 390, 500`).

Consequences:
- A `lookup_symbol` or `get_proof_context` call during a long typecheck blocks until the
  typecheck finishes — so the agent cannot inspect a type or a proof state while waiting,
  which is exactly when it wants to.
- The lock is over the *whole session map*, so a slow check in session A blocks every
  operation on unrelated session B.

**Needed:** per-session locking at minimum; better, route read-only queries to a companion
lax process (#9) so they never contend with the verifying process at all.

### 9. No lax companion process

`Session` holds exactly one `FStarProcess` (`src/session/mod.rs`), spawned with `lax: false`
(`Session::new(..., false)` in `create_session`). The `lax` flag on `typecheck_buffer` sets
the full-buffer *kind*, which is not the same thing as a second process running with
`--admit_smt_queries true`.

Editors run **two** processes per document: one for real verification, one lax process for
sub-second type/syntax feedback, symbol lookup and completion
(`documentState.ts`: `FStarDocumentState.fstar` / `.fstar_lax`; `DESIGN.md`). Agents want
this even more than humans do: `lax` after every edit for fast type errors, full SMT
verification only when the shape is right.

**Needed:** spawn the pair, route lookups to the lax process, and merge diagnostics —
using lax results only *beyond* the point the full process has verified, and downgrading
their severity (see `FStarDocumentState.diagnosticsRateLimiter`, which does exactly this).

### 10. No cache-reuse reporting, and no edit invalidation tracking

`FullBufferResult` returns fragments as ranges plus a status
(`src/fstar/process.rs`, `src/session/types.rs`: `FragmentInfo`). Nothing distinguishes a
fragment that F* *replayed from cache* from one it actually re-checked, and there is no
equivalent of the extension's `invalidatedThroughEdits` bookkeeping
(`documentState.ts`: `invalidateResults`, `FragmentResult.invalidatedThroughEdits`, and the
`newResults` double-buffer that separates the cached-replay phase from real work).

Consequence, and it's a behavioural one: the agent gets **no feedback that its editing
style is destroying the cache**. An agent that rewrites a file top-to-bottom (which they do
by default) invalidates every fragment and pays full batch cost while believing it is using
interactive mode. It has no way to notice.

**Needed:** report `reused N/M fragments` and `verified through line L` on every check, and
emit an explicit hint when reuse is 0 after a prior successful check
("whole-file rewrite discarded the verified prefix — prefer targeted edits below line N").
This is cheap to implement and is the main lever for teaching agents the right workflow.

### 11. No staleness detection

Nothing hashes the file. A result is returned with no evidence of *which* text it
corresponds to — and given #2, the checked text came from the model, not from disk at all.

**Needed:** hash at check time, compare on completion, and mark results stale if the file
changed underneath. Report the hash/mtime alongside the verdict.

### 12. No crash recovery

If `fstar.exe` dies (OOM, internal error, killed Z3), the reader task hits EOF and every
subsequent query fails with `ProcessExited` (`process.rs`). The session is dead; the agent
sees an opaque error.

**Needed:** detect the exit, respawn transparently, and report
`process crashed, cache lost, re-verifying from top` — degraded but recoverable, rather
than a hard failure the agent doesn't know how to react to.

### 13. No resource ceilings

Sessions are unbounded (`SessionManager` is a plain `HashMap`), and each holds an entire
elaborated dependency graph — hundreds of MB on projects like HACL*/Steel/Pulse. Cleanup is
only by explicit `close_session`, by an optional per-session `timeout` passed at creation,
or by the MCP-session-close sweeper (`mark_sessions_for_deletion` / `sweep_marked_sessions`,
default 300s).

Consequence: an agent working across a dozen files will exhaust memory, and OOM in F* is
not graceful.

**Needed:** a cap on concurrent sessions (a small default, e.g. 4 process pairs), LRU +
idle eviction independent of the MCP session lifecycle, and eviction visible in responses so
the agent understands why a check suddenly got slow.

---

## P3 — Interface and ergonomics

### 14. Explicit `session_id` threading

`create_session` returns a UUID that the model must carry and pass to all seven other tools.
Models lose this routinely — they call `typecheck_buffer` with a stale or invented id, get
`session_not_found` (`tools.rs:24`), and burn turns recovering. `list_sessions` exists
largely to paper over this.

**Needed:** implicit sessions keyed by canonical file path. First check on a path spawns and
configures the processes; later calls reuse them. Keep an explicit `restart` as the escape
hatch. This also removes the need for `create_session`, `close_session` and `list_sessions`
as model-facing tools — 8 tools becomes ~5, which is a real context saving.

### 15. HTTP-only transport, fixed port, global singleton state

`main.rs` binds `StreamableHttpServer` to `127.0.0.1:3000` (overridable only via
`FSTAR_MCP_PORT`), and session state lives in a `lazy_static` `SESSION_MANAGER`.

Consequences: stdio is the default transport for the agent hosts most people use (Copilot
CLI, Claude Code) and is not supported; the fixed port means two workspaces collide; and a
single global process means no per-workspace isolation of F* configuration — see #19 for
the concrete multi-client hazards this creates.

**Needed:** stdio transport as the primary mode (HTTP optional for multi-client/daemon use),
and workspace-scoped state.

### 16. Token-heavy JSON responses; lossy diagnostics

Responses are raw JSON with a full `{start_line, start_column, end_line, end_column}` object
per fragment and per diagnostic (`src/session/types.rs`). A 300-fragment file produces a
large array on **every** check, most of it unchanged and unread.

At the same time it *loses* information: `DiagnosticInfo::from` keeps only
`diag.ranges.first()` and drops the rest, so F*'s "see also" related locations — often the
most useful part of a failed proof obligation — are discarded. It also drops
`IdeDiagnostic.number` (`src/fstar/messages.rs:62`), the error code.

**Needed:** compact, human/agent-readable text: a one-line verdict with timing and reuse
stats, `file:line:col: error <number>: message` per diagnostic with related locations
indented beneath, a capped diagnostic count, and a summary rather than a full fragment dump
(with fragment detail available on request via a separate status tool).

### 17. `create_session` always does a full verification

`create_session` unconditionally reads the file and runs a **full** typecheck
(`tools.rs:98-107`) — no lax option, no `to_position`, no way to just warm the process.

Consequence: the first call on a large module blocks for the full batch duration, which is
precisely the cost the tool exists to avoid, and it happens before the agent has made any
edit. Combined with #7 (no timeout) this is where an agent's first interaction most often
hangs.

**Needed:** separate "warm the process / load dependencies" from "verify", and default the
first contact to the cheap option.

### 18. No integration tests against a real `fstar.exe`

`tests/mcp_client_tests.rs` exercises a hand-written `MockFStarSessionManager`
(`tests/mock_fstar.rs`) — it tests the mock's bookkeeping, not the protocol handling. The
query-id bug (#5) is exactly the class of defect a mock cannot catch, and indeed it is
present.

**Needed:** tests that drive a real `fstar.exe --ide` over a small fixture module, covering:
cache reuse across edits, cancellation mid-proof, a failing proof obligation, interleaved
lookup during a long check, and process death/recovery.

---

## Suggested order of work

1. **#2 (disk as source of truth)** and **#1 (config discovery)** — until both land, the
   server can't be used on a real project and the token economics are upside down.
2. **#5, #6** — correctness bugs; #5 in particular will produce confusing wrong results.
   Add **#19** here too *if more than one client will ever connect*: cross-client session
   eviction is silent and looks like poor performance rather than a bug.
3. **#3 (cancel)**, **#7 (timeouts + partial results)**, **#8 (locking)** — these are what
   make the loop feel interactive rather than merely warm.
4. **#10 (reuse reporting)** — cheap, and the main thing that will change agent behaviour.
5. **#9 (lax pair)**, **#14 (implicit sessions)**, **#15 (stdio)**, **#16 (compact output)**.
   #14 and #19 should be designed together — implicit path-keyed sessions force the
   question of what happens when two clients key on the same path.
6. **#4 (dependency awareness)**, **#11–13 (staleness, crash recovery, limits)**,
   **#18 (real integration tests)**.

## Note on duplication

Items #1, #3, #5, #6, #9, #10 are all *already implemented and battle-tested* in
`fstar-vscode-assistant`'s `lspserver/` (`fstar.ts`, `fstar_connection.ts`,
`documentState.ts`) — that code has absorbed years of F* IDE-protocol quirks. Reimplementing
them in Rust means maintaining two independent encodings of an undocumented protocol; #5 is
what that already cost. Worth deciding explicitly whether to port this repo's core (and have
fstar-mcp become a thin front end over it) before spending the effort twice.
