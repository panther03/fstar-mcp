//! F* IDE process management.

use crate::fstar::config::FStarConfig;
use crate::fstar::messages::*;
use crate::fstar::protocol::{parse_response, FStarResponse, JsonlInterface};
use crate::is_verbose;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

#[derive(Error, Debug)]
pub enum ProcessError {
    #[error("Failed to spawn F* process: {0}")]
    SpawnError(#[from] std::io::Error),
    #[error("F* executable not found: {0}")]
    ExecutableNotFound(String),
    #[error("F* process exited unexpectedly with code {0:?}")]
    ProcessExited(Option<i32>),
    #[error("Failed to send message to F*: {0}")]
    SendError(String),
    #[error("F* does not support full-buffer mode")]
    NoFullBufferSupport,
    #[error("Query timed out")]
    Timeout,
}

/// Result of a full-buffer query
#[derive(Debug, Clone, Default)]
pub struct FullBufferResult {
    pub diagnostics: Vec<IdeDiagnostic>,
    pub fragments: Vec<FragmentResult>,
    pub proof_states: Vec<IdeProofState>,
    pub finished: bool,
    pub timed_out: bool,
}

/// Result for a single fragment
#[derive(Debug, Clone)]
pub struct FragmentResult {
    pub range: FStarRange,
    pub status: FragmentStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FragmentStatus {
    Ok,
    LaxOk,
    Failed,
    InProgress,
}

/// Manages a single F* IDE process
pub struct FStarProcess {
    child: Child,
    jsonl: JsonlInterface,
    query_id: Arc<AtomicU64>,
    response_rx: mpsc::Receiver<FStarResponse>,
    pending_responses: VecDeque<FStarResponse>,
    pub supports_full_buffer: bool,
    pub ide_version: i32,
}

#[derive(Clone)]
pub struct FStarProcessControl {
    jsonl: JsonlInterface,
    query_id: Arc<AtomicU64>,
}

impl FStarProcessControl {
    fn next_query_id(&self) -> String {
        self.query_id.fetch_add(1, Ordering::SeqCst).to_string()
    }

    pub async fn cancel(&self, position: (u32, u32)) -> Result<bool, ProcessError> {
        let query = serde_json::json!({
            "query-id": self.next_query_id(),
            "query": "cancel",
            "args": {
                "cancel-line": position.0,
                "cancel-column": position.1
            }
        });
        self.jsonl
            .send_message(&query)
            .await
            .map_err(|e| ProcessError::SendError(e.to_string()))?;
        Ok(true)
    }
}

impl FStarProcess {
    /// Spawn a new F* IDE process
    pub async fn spawn(
        config: FStarConfig,
        file_path: &Path,
        lax: bool,
    ) -> Result<Self, ProcessError> {
        let fstar_exe = config.fstar_exe().to_string();
        let cwd = config.cwd_or(file_path.parent().unwrap_or(Path::new(".")));
        let args = config.build_args(&file_path.to_string_lossy(), lax);

        if is_verbose() {
            tracing::info!(
                "[F* spawn] {} {} (cwd: {:?})",
                fstar_exe,
                args.join(" "),
                cwd
            );
        } else {
            tracing::debug!("Spawning F* with args: {:?} in {:?}", args, cwd);
        }

        let mut attempts = 0;
        let mut child = loop {
            match Command::new(&fstar_exe)
                .args(&args)
                .current_dir(&cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => break child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ProcessError::ExecutableNotFound(fstar_exe.clone()));
                }
                Err(error) if attempts < 3 && is_transient_spawn_error(&error) => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(ProcessError::SpawnError(error)),
            }
        };

        let stdin = child.stdin.take().expect("stdin not captured");
        let stdout = child.stdout.take().expect("stdout not captured");
        let stderr = child.stderr.take().expect("stderr not captured");

        // Set up response channel
        let (tx, rx) = mpsc::channel(100);

        // Capture verbose flag for async tasks
        let verbose = Arc::new(AtomicBool::new(is_verbose()));

        // Spawn stdout reader task
        let tx_clone = tx.clone();
        let verbose_stdout = verbose.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        if verbose_stdout.load(Ordering::Relaxed) {
                            tracing::info!("[F* → MCP] <EOF>");
                        }
                        break; // EOF
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        if verbose_stdout.load(Ordering::Relaxed) {
                            tracing::info!("[F* → MCP] {}", trimmed);
                        }

                        match parse_response(trimmed) {
                            Ok(response) => {
                                if tx_clone.send(response).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to parse F* response: {} | raw: {}",
                                    e,
                                    trimmed
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error reading F* stdout: {}", e);
                        break;
                    }
                }
            }
        });

        // Spawn stderr reader task (always log stderr)
        let verbose_stderr = verbose.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            if verbose_stderr.load(Ordering::Relaxed) {
                                tracing::info!("[F* stderr] {}", trimmed);
                            } else {
                                tracing::warn!("F* stderr: {}", trimmed);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let jsonl = JsonlInterface::new(stdin);

        let mut process = FStarProcess {
            child,
            jsonl,
            query_id: Arc::new(AtomicU64::new(1)),
            response_rx: rx,
            pending_responses: VecDeque::new(),
            supports_full_buffer: true, // Assume true until we get protocol-info
            ide_version: 3,
        };

        // Wait for protocol-info
        process.wait_for_protocol_info().await?;

        Ok(process)
    }

    /// Wait for the initial protocol-info message
    async fn wait_for_protocol_info(&mut self) -> Result<(), ProcessError> {
        // Give F* some time to start up
        let timeout = tokio::time::Duration::from_secs(30);
        let result = tokio::time::timeout(timeout, self.response_rx.recv()).await;

        match result {
            Ok(Some(FStarResponse::ProtocolInfo(info))) => {
                self.supports_full_buffer = info.supports_full_buffer();
                self.ide_version = info.version;
                tracing::info!(
                    "F* protocol version: {}, full-buffer: {}",
                    info.version,
                    self.supports_full_buffer
                );
                Ok(())
            }
            Ok(Some(other)) => {
                tracing::warn!("Expected protocol-info, got: {:?}", other);
                Ok(()) // Continue anyway
            }
            Ok(None) => Err(ProcessError::ProcessExited(None)),
            Err(_) => Err(ProcessError::Timeout),
        }
    }

    /// Get the next query ID
    fn next_query_id(&self) -> String {
        self.query_id.fetch_add(1, Ordering::SeqCst).to_string()
    }

    pub fn control(&self) -> FStarProcessControl {
        FStarProcessControl {
            jsonl: self.jsonl.clone(),
            query_id: self.query_id.clone(),
        }
    }

    pub fn has_exited(&mut self) -> Result<bool, ProcessError> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(ProcessError::SpawnError)
    }

    /// Send a query and return the query ID
    pub async fn send_query(&self, mut query: serde_json::Value) -> Result<String, ProcessError> {
        let qid = self.next_query_id();
        query["query-id"] = serde_json::Value::String(qid.clone());

        tracing::debug!("Sending query: {}", serde_json::to_string(&query).unwrap());

        self.jsonl
            .send_message(&query)
            .await
            .map_err(|e| ProcessError::SendError(e.to_string()))?;

        Ok(qid)
    }

    /// Send a full-buffer query and collect all responses until finished
    pub async fn full_buffer_query(
        &mut self,
        code: &str,
        kind: &str,
        to_position: Option<(u32, u32)>,
        timeout: Duration,
    ) -> Result<FullBufferResult, ProcessError> {
        if !self.supports_full_buffer {
            return Err(ProcessError::NoFullBufferSupport);
        }

        let mut query = serde_json::json!({
            "query": "full-buffer",
            "args": {
                "kind": kind,
                "with-symbols": false,
                "code": code,
                "line": 0,
                "column": 0
            }
        });

        if let Some((line, col)) = to_position {
            query["args"]["to-position"] = serde_json::json!({
                "line": line,
                "column": col
            });
        }

        let qid = self.send_query(query).await?;
        let mut result = FullBufferResult::default();
        let deadline = Instant::now() + timeout;

        // Collect responses until full-buffer-finished
        loop {
            let response = self.recv_matching(&qid, true, deadline).await?;
            let Some(response) = response else {
                result.timed_out = true;
                let position = result
                    .fragments
                    .last()
                    .map(|fragment| fragment.range.beg)
                    .unwrap_or((1, 0));
                self.control().cancel(position).await?;
                break;
            };

            match response {
                FStarResponse::Progress {
                    query_id: _,
                    stage,
                    ranges,
                } => match stage.as_str() {
                    "full-buffer-started" => {
                        tracing::debug!("Full buffer started");
                    }
                    "full-buffer-finished" => {
                        result.finished = true;
                        break;
                    }
                    "full-buffer-fragment-started" => {
                        Self::record_fragment(
                            &mut result.fragments,
                            ranges,
                            FragmentStatus::InProgress,
                        );
                    }
                    "full-buffer-fragment-ok" => {
                        Self::record_fragment(&mut result.fragments, ranges, FragmentStatus::Ok);
                    }
                    "full-buffer-fragment-lax-ok" => {
                        Self::record_fragment(&mut result.fragments, ranges, FragmentStatus::LaxOk);
                    }
                    "full-buffer-fragment-failed" => {
                        Self::record_fragment(
                            &mut result.fragments,
                            ranges,
                            FragmentStatus::Failed,
                        );
                    }
                    _ => {}
                },
                FStarResponse::Response(resp) => {
                    // Check for diagnostics in response
                    if let Some(response) = &resp.response {
                        if let Ok(diags) =
                            serde_json::from_value::<Vec<IdeDiagnostic>>(response.clone())
                        {
                            result.diagnostics.extend(diags);
                        }
                    }
                }
                FStarResponse::ProofState { proof_state, .. } => {
                    result.proof_states.push(proof_state);
                }
                FStarResponse::StatusMessage {
                    level, contents, ..
                } => {
                    tracing::debug!("F* {}: {}", level, contents);
                }
                FStarResponse::ProtocolInfo(_) => {
                    // Ignore late protocol info
                }
            }
        }

        Ok(result)
    }

    fn record_fragment(
        fragments: &mut Vec<FragmentResult>,
        range: Option<FStarRange>,
        status: FragmentStatus,
    ) {
        if let Some(range) = range {
            if let Some(fragment) = fragments.iter_mut().rev().find(|f| f.range == range) {
                fragment.status = status;
            } else {
                fragments.push(FragmentResult { range, status });
            }
        } else if let Some(fragment) = fragments.last_mut() {
            fragment.status = status;
        }
    }

    async fn recv_matching(
        &mut self,
        query_id: &str,
        include_subqueries: bool,
        deadline: Instant,
    ) -> Result<Option<FStarResponse>, ProcessError> {
        if let Some(index) = self.pending_responses.iter().position(|response| {
            response_query_id(response)
                .map(|response_id| query_id_matches(response_id, query_id, include_subqueries))
                .unwrap_or(false)
        }) {
            return Ok(self.pending_responses.remove(index));
        }

        loop {
            let response = match tokio::time::timeout_at(deadline, self.response_rx.recv()).await {
                Ok(Some(response)) => response,
                Ok(None) => return Err(ProcessError::ProcessExited(None)),
                Err(_) => return Ok(None),
            };

            if response_query_id(&response)
                .map(|response_id| query_id_matches(response_id, query_id, include_subqueries))
                .unwrap_or(false)
            {
                return Ok(Some(response));
            }

            if self.pending_responses.len() == 1024 {
                tracing::warn!("Dropping oldest unmatched F* response");
                self.pending_responses.pop_front();
            }
            self.pending_responses.push_back(response);
        }
    }

    /// Send a vfs-add query
    pub async fn vfs_add(
        &mut self,
        filename: Option<&str>,
        contents: &str,
    ) -> Result<(), ProcessError> {
        let query = serde_json::json!({
            "query": "vfs-add",
            "args": {
                "filename": filename,
                "contents": contents
            }
        });

        let qid = self.send_query(query).await?;

        let deadline = Instant::now() + Duration::from_secs(15);
        if let Some(FStarResponse::Response(_)) = self.recv_matching(&qid, false, deadline).await? {
            return Ok(());
        }
        Err(ProcessError::Timeout)
    }

    /// Send a lookup query
    pub async fn lookup(
        &mut self,
        filename: &str,
        line: u32,
        column: u32,
        symbol: &str,
    ) -> Result<Option<IdeLookupResponse>, ProcessError> {
        let query = serde_json::json!({
            "query": "lookup",
            "args": {
                "context": "code",
                "symbol": symbol,
                "requested-info": ["type", "documentation", "defined-at"],
                "location": {
                    "filename": filename,
                    "line": line,
                    "column": column
                }
            }
        });

        let qid = self.send_query(query).await?;

        let deadline = Instant::now() + Duration::from_secs(15);
        if let Some(FStarResponse::Response(resp)) =
            self.recv_matching(&qid, false, deadline).await?
        {
            if resp.status.as_deref() == Some("success") {
                if let Some(r) = resp.response {
                    return Ok(serde_json::from_value(r).ok());
                }
            }
            return Ok(None);
        }

        Err(ProcessError::Timeout)
    }

    #[cfg(unix)]
    async fn kill_z3_descendants(&self) {
        let Some(root_pid) = self.child.id() else {
            return;
        };
        let output = match Command::new("ps")
            .args(["-eo", "pid=,ppid=,comm="])
            .output()
            .await
        {
            Ok(output) if output.status.success() => output,
            _ => {
                tracing::warn!("Could not enumerate F* child processes");
                return;
            }
        };

        let mut processes = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.split_whitespace();
            let (Some(pid), Some(ppid), Some(command)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                processes.push((pid, ppid, command.to_string()));
            }
        }

        let mut descendants = vec![root_pid];
        let mut index = 0;
        while index < descendants.len() {
            let parent = descendants[index];
            for (pid, ppid, _) in &processes {
                if *ppid == parent && !descendants.contains(pid) {
                    descendants.push(*pid);
                }
            }
            index += 1;
        }

        for (pid, _, command) in processes {
            if descendants.contains(&pid) && command.starts_with("z3") {
                tracing::info!(pid, "Terminating wedged Z3 child");
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status()
                    .await;
            }
        }
    }

    #[cfg(not(unix))]
    async fn kill_z3_descendants(&self) {}

    /// Send restart-solver request after terminating any wedged Z3 children.
    pub async fn restart_solver(&mut self) -> Result<(), ProcessError> {
        self.kill_z3_descendants().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let query = serde_json::json!({
            "query": "restart-solver",
            "args": {}
        });

        self.send_query(query).await?;
        Ok(())
    }

    /// Kill the F* process.
    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        if self.has_exited()? {
            return Ok(());
        }
        self.child.kill().await.map_err(ProcessError::SpawnError)
    }
}

fn response_query_id(response: &FStarResponse) -> Option<&str> {
    match response {
        FStarResponse::Response(response) => Some(&response.query_id),
        FStarResponse::Progress { query_id, .. }
        | FStarResponse::ProofState { query_id, .. }
        | FStarResponse::StatusMessage { query_id, .. } => Some(query_id),
        FStarResponse::ProtocolInfo(_) => None,
    }
}

fn query_id_matches(response_id: &str, query_id: &str, include_subqueries: bool) -> bool {
    if include_subqueries {
        response_id.split('.').next() == Some(query_id)
    } else {
        response_id == query_id
    }
}

#[cfg(unix)]
fn is_transient_spawn_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(26)
}

#[cfg(not(unix))]
fn is_transient_spawn_error(_error: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::query_id_matches;

    #[test]
    fn query_ids_only_match_exact_roots() {
        assert!(query_id_matches("1", "1", true));
        assert!(query_id_matches("1.12", "1", true));
        assert!(!query_id_matches("10", "1", true));
        assert!(!query_id_matches("10.1", "1", true));
        assert!(!query_id_matches("1.1", "1", false));
    }
}

impl Drop for FStarProcess {
    fn drop(&mut self) {
        // Try to kill the process when dropped
        let _ = self.child.start_kill();
    }
}
