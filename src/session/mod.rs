//! Session management for F* MCP server.

pub mod types;

use crate::fstar::{
    FStarConfig, FStarProcess, FStarProcessControl, FragmentResult, FullBufferResult,
    IdeProofState, ProcessError,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

pub use types::*;

pub const DEFAULT_SWEEP_PERIOD_SECS: u64 = 300;
pub const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_MAX_SESSIONS: usize = 4;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;
type SessionKey = (String, PathBuf, String);

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("Session {session_id} belongs to another MCP client")]
    NotOwned { session_id: String },
    #[error("Failed to create session: {0}")]
    CreateError(#[from] ProcessError),
    #[error("Config error: {0}")]
    ConfigError(#[from] crate::fstar::ConfigError),
}

#[derive(Default)]
struct ProcessState {
    last_code: Option<String>,
    last_fragments: Vec<FragmentResult>,
    dependency_hashes: HashMap<PathBuf, String>,
    dependencies: Vec<PathBuf>,
    dependency_scan_complete: bool,
}

#[derive(Default)]
struct SessionState {
    last_activity: DateTime<Utc>,
    proof_states: Vec<IdeProofState>,
    checker: ProcessState,
    lax: ProcessState,
    marked_for_deletion: bool,
}

pub struct Session {
    pub id: String,
    pub file_path: PathBuf,
    pub config: FStarConfig,
    pub created_at: DateTime<Utc>,
    pub mcp_session_id: Option<String>,
    checker: Mutex<FStarProcess>,
    checker_control: RwLock<FStarProcessControl>,
    lax: Mutex<FStarProcess>,
    lax_control: RwLock<FStarProcessControl>,
    state: RwLock<SessionState>,
    cache_lost: AtomicBool,
}

pub struct SessionCheckResult {
    pub result: FullBufferResult,
    pub content_hash: String,
    pub stale: bool,
    pub reused_fragments: usize,
    pub verified_through_line: u32,
    pub hint: Option<String>,
    pub process_restarted: bool,
    pub dependencies_changed: Vec<String>,
}

impl Session {
    pub async fn new(
        file_path: &Path,
        config: FStarConfig,
        mcp_session_id: Option<String>,
        cache_lost: bool,
    ) -> Result<Self, SessionError> {
        let checker = FStarProcess::spawn(config.clone(), file_path, false).await?;
        let checker_control = checker.control();
        let lax = FStarProcess::spawn(config.clone(), file_path, true).await?;
        let lax_control = lax.control();
        let now = Utc::now();

        Ok(Self {
            id: Uuid::new_v4().to_string(),
            file_path: file_path.to_path_buf(),
            config,
            created_at: now,
            mcp_session_id,
            checker: Mutex::new(checker),
            checker_control: RwLock::new(checker_control),
            lax: Mutex::new(lax),
            lax_control: RwLock::new(lax_control),
            state: RwLock::new(SessionState {
                last_activity: now,
                ..SessionState::default()
            }),
            cache_lost: AtomicBool::new(cache_lost),
        })
    }

    pub async fn check(
        &self,
        code: &str,
        requested_kind: &str,
        to_position: Option<(u32, u32)>,
        timeout: Duration,
        disk_hash: Option<&str>,
    ) -> Result<SessionCheckResult, ProcessError> {
        let use_lax = matches!(requested_kind, "lax" | "lax-to-position");
        let content_hash = content_hash(code);
        let (known_dependencies, run_fstar_dep) = {
            let state = self.state.read().await;
            let process_state = if use_lax { &state.lax } else { &state.checker };
            (
                process_state.dependencies.clone(),
                !process_state.dependency_scan_complete,
            )
        };
        let dependencies = discover_dependencies(
            &self.file_path,
            &self.config,
            code,
            known_dependencies,
            run_fstar_dep,
        )
        .await;
        let dependency_hashes = hash_files(&dependencies).await;

        let (diff_position, previous_fragments, dependencies_changed) = {
            let mut state = self.state.write().await;
            state.last_activity = Utc::now();
            let process_state = if use_lax {
                &mut state.lax
            } else {
                &mut state.checker
            };
            let had_previous_check = process_state.last_code.is_some();
            let diff = process_state
                .last_code
                .as_deref()
                .and_then(|previous| first_diff_position(previous, code));
            let changed = if had_previous_check {
                changed_dependencies(&process_state.dependency_hashes, &dependency_hashes)
            } else {
                Vec::new()
            };
            let previous_fragments = process_state.last_fragments.clone();
            process_state.last_code = Some(code.to_string());
            (diff, previous_fragments, changed)
        };

        if let Some(position) = diff_position {
            let control = if use_lax {
                &self.lax_control
            } else {
                &self.checker_control
            };
            if let Err(error) = control.read().await.cancel(position).await {
                tracing::warn!(%error, "Could not send rollback before checking process health");
            }
        }

        let reload_dependencies =
            !dependencies_changed.is_empty() && matches!(requested_kind, "full" | "lax");

        let mut process_restarted = self.cache_lost.swap(false, Ordering::SeqCst);
        let result = if use_lax {
            let mut process = self.lax.lock().await;
            if process.has_exited()? {
                *process = FStarProcess::spawn(self.config.clone(), &self.file_path, true).await?;
                *self.lax_control.write().await = process.control();
                process_restarted = true;
            }
            if reload_dependencies {
                process
                    .full_buffer_query(code, "reload-deps", None, timeout)
                    .await?;
            }
            match process
                .full_buffer_query(code, requested_kind, to_position, timeout)
                .await
            {
                Ok(result) => result,
                Err(ProcessError::ProcessExited(_) | ProcessError::SendError(_)) => {
                    *process =
                        FStarProcess::spawn(self.config.clone(), &self.file_path, true).await?;
                    *self.lax_control.write().await = process.control();
                    process_restarted = true;
                    if reload_dependencies {
                        process
                            .full_buffer_query(code, "reload-deps", None, timeout)
                            .await?;
                    }
                    process
                        .full_buffer_query(code, requested_kind, to_position, timeout)
                        .await?
                }
                Err(error) => return Err(error),
            }
        } else {
            let mut process = self.checker.lock().await;
            if process.has_exited()? {
                *process = FStarProcess::spawn(self.config.clone(), &self.file_path, false).await?;
                *self.checker_control.write().await = process.control();
                process_restarted = true;
            }
            if reload_dependencies {
                process
                    .full_buffer_query(code, "reload-deps", None, timeout)
                    .await?;
            }
            match process
                .full_buffer_query(code, requested_kind, to_position, timeout)
                .await
            {
                Ok(result) => result,
                Err(ProcessError::ProcessExited(_) | ProcessError::SendError(_)) => {
                    *process =
                        FStarProcess::spawn(self.config.clone(), &self.file_path, false).await?;
                    *self.checker_control.write().await = process.control();
                    process_restarted = true;
                    if reload_dependencies {
                        process
                            .full_buffer_query(code, "reload-deps", None, timeout)
                            .await?;
                    }
                    process
                        .full_buffer_query(code, requested_kind, to_position, timeout)
                        .await?
                }
                Err(error) => return Err(error),
            }
        };

        let reused_fragments = count_reused_prefix(&previous_fragments, &result.fragments);
        let verified_through_line = result
            .fragments
            .iter()
            .filter(|fragment| {
                matches!(
                    fragment.status,
                    crate::fstar::FragmentStatus::Ok | crate::fstar::FragmentStatus::LaxOk
                )
            })
            .map(|fragment| fragment.range.end.0)
            .max()
            .unwrap_or(0);
        let hint = if !previous_fragments.is_empty()
            && reused_fragments == 0
            && result.fragments.len() > 1
        {
            Some(
                "No verified prefix was reused. Prefer targeted edits below the last verified line."
                    .to_string(),
            )
        } else {
            None
        };

        let current_disk_hash = hash_file(&self.file_path).await;
        let stale = disk_hash
            .zip(current_disk_hash.as_deref())
            .map(|(before, after)| before != after)
            .unwrap_or(false);

        {
            let mut state = self.state.write().await;
            state.last_activity = Utc::now();
            if !use_lax {
                state.proof_states = result.proof_states.clone();
            }
            let process_state = if use_lax {
                &mut state.lax
            } else {
                &mut state.checker
            };
            process_state.last_fragments = result.fragments.clone();
            process_state.dependency_hashes = dependency_hashes;
            process_state.dependencies = dependencies;
            process_state.dependency_scan_complete = true;
        }

        Ok(SessionCheckResult {
            result,
            content_hash,
            stale,
            reused_fragments,
            verified_through_line,
            hint,
            process_restarted,
            dependencies_changed,
        })
    }

    pub async fn lookup(
        &self,
        filename: &str,
        line: u32,
        column: u32,
        symbol: &str,
    ) -> Result<Option<crate::fstar::IdeLookupResponse>, ProcessError> {
        self.state.write().await.last_activity = Utc::now();
        let code = tokio::fs::read_to_string(&self.file_path)
            .await
            .map_err(ProcessError::SpawnError)?;
        let (known_dependencies, run_fstar_dep, previous_hashes, initialized) = {
            let state = self.state.read().await;
            (
                state.lax.dependencies.clone(),
                !state.lax.dependency_scan_complete,
                state.lax.dependency_hashes.clone(),
                state.lax.dependency_scan_complete,
            )
        };
        let dependencies = discover_dependencies(
            &self.file_path,
            &self.config,
            &code,
            known_dependencies,
            run_fstar_dep,
        )
        .await;
        let dependency_hashes = hash_files(&dependencies).await;
        let dependencies_changed =
            initialized && !changed_dependencies(&previous_hashes, &dependency_hashes).is_empty();
        let mut lax = self.lax.lock().await;
        let mut process_restarted = false;
        if lax.has_exited()? {
            *lax = FStarProcess::spawn(self.config.clone(), &self.file_path, true).await?;
            *self.lax_control.write().await = lax.control();
            process_restarted = true;
        }
        if dependencies_changed {
            lax.full_buffer_query(
                &code,
                "reload-deps",
                None,
                Duration::from_secs(DEFAULT_QUERY_TIMEOUT_SECS),
            )
            .await?;
        }
        let initialization_result = if !initialized || dependencies_changed || process_restarted {
            Some(
                lax.full_buffer_query(
                    &code,
                    "lax",
                    None,
                    Duration::from_secs(DEFAULT_QUERY_TIMEOUT_SECS),
                )
                .await?,
            )
        } else {
            None
        };
        {
            let mut state = self.state.write().await;
            state.lax.last_code = Some(code);
            state.lax.dependencies = dependencies;
            state.lax.dependency_hashes = dependency_hashes;
            state.lax.dependency_scan_complete = true;
            if let Some(result) = initialization_result {
                state.lax.last_fragments = result.fragments;
            }
        }
        lax.lookup(filename, line, column, symbol).await
    }

    pub async fn vfs_add(
        &self,
        filename: Option<&str>,
        contents: &str,
    ) -> Result<(), ProcessError> {
        self.state.write().await.last_activity = Utc::now();
        let mut checker = self.checker.lock().await;
        checker.vfs_add(filename, contents).await?;
        drop(checker);
        self.lax.lock().await.vfs_add(filename, contents).await
    }

    pub async fn restart_solver(&self) -> Result<(), ProcessError> {
        self.state.write().await.last_activity = Utc::now();
        let mut checker = self.checker.lock().await;
        checker.restart_solver().await?;
        drop(checker);
        self.lax.lock().await.restart_solver().await
    }

    pub async fn proof_states(&self) -> Vec<IdeProofState> {
        self.state.read().await.proof_states.clone()
    }

    pub async fn fragments(&self) -> Vec<FragmentResult> {
        let state = self.state.read().await;
        if state.checker.last_fragments.is_empty() {
            state.lax.last_fragments.clone()
        } else {
            state.checker.last_fragments.clone()
        }
    }

    pub async fn cancel_current(
        &self,
        lax: bool,
        position: (u32, u32),
    ) -> Result<(), ProcessError> {
        let control = if lax {
            &self.lax_control
        } else {
            &self.checker_control
        };
        control.read().await.cancel(position).await?;
        Ok(())
    }

    async fn kill(&self) {
        let _ = self.checker.lock().await.kill().await;
        let _ = self.lax.lock().await.kill().await;
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub file_path: String,
    pub created_at: String,
    pub last_activity: String,
    pub idle_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_session_id: Option<String>,
    pub marked_for_deletion: bool,
}

pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
    file_to_session: Arc<RwLock<HashMap<SessionKey, String>>>,
    mcp_to_fstar_sessions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    timed_out_sessions: Arc<RwLock<HashMap<String, u64>>>,
    max_sessions: usize,
    idle_timeout_secs: u64,
    evicted_keys: RwLock<HashSet<SessionKey>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        let max_sessions = std::env::var("FSTAR_MCP_MAX_SESSIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_MAX_SESSIONS)
            .max(1);
        let idle_timeout_secs = std::env::var("FSTAR_MCP_IDLE_TIMEOUT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            file_to_session: Arc::new(RwLock::new(HashMap::new())),
            mcp_to_fstar_sessions: Arc::new(RwLock::new(HashMap::new())),
            timed_out_sessions: Arc::new(RwLock::new(HashMap::new())),
            max_sessions,
            idle_timeout_secs,
            evicted_keys: RwLock::new(HashSet::new()),
        }
    }

    fn owner_key(mcp_session_id: Option<&str>) -> String {
        mcp_session_id.unwrap_or("stdio").to_string()
    }

    pub async fn get_timeout_info(&self, session_id: &str) -> Option<u64> {
        self.timed_out_sessions
            .read()
            .await
            .get(session_id)
            .copied()
    }

    pub async fn find_by_path(
        &self,
        file_path: &Path,
        mcp_session_id: Option<&str>,
        config: &FStarConfig,
    ) -> Option<Arc<Session>> {
        let canonical = canonical_path(file_path).await;
        let key = (
            Self::owner_key(mcp_session_id),
            canonical,
            config_fingerprint(config),
        );
        let id = self.file_to_session.read().await.get(&key).cloned()?;
        self.sessions.read().await.get(&id).cloned()
    }

    pub async fn get_owned(
        &self,
        session_id: &str,
        mcp_session_id: Option<&str>,
    ) -> Result<Arc<Session>, SessionError> {
        let session = self
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if session.mcp_session_id.as_deref() != mcp_session_id {
            return Err(SessionError::NotOwned {
                session_id: session_id.to_string(),
            });
        }
        Ok(session)
    }

    pub async fn create_session(
        &self,
        file_path: &Path,
        config: FStarConfig,
        mcp_session_id: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<Arc<Session>, SessionError> {
        let canonical = canonical_path(file_path).await;
        if let Some(session) = self
            .find_by_path(&canonical, mcp_session_id.as_deref(), &config)
            .await
        {
            return Ok(session);
        }

        self.evict_lru_if_needed().await;
        let config_key = config_fingerprint(&config);
        let key = (
            Self::owner_key(mcp_session_id.as_deref()),
            canonical.clone(),
            config_key,
        );
        let cache_lost = self.evicted_keys.write().await.remove(&key);
        let session =
            Arc::new(Session::new(&canonical, config, mcp_session_id.clone(), cache_lost).await?);
        let session_id = session.id.clone();
        self.sessions
            .write()
            .await
            .insert(session_id.clone(), session.clone());
        self.file_to_session
            .write()
            .await
            .insert(key, session_id.clone());
        if let Some(owner) = mcp_session_id {
            self.mcp_to_fstar_sessions
                .write()
                .await
                .entry(owner)
                .or_default()
                .insert(session_id.clone());
        }

        if let Some(seconds) = timeout_secs {
            let timed_out = self.timed_out_sessions.clone();
            let sessions = self.sessions.clone();
            let file_to_session = self.file_to_session.clone();
            let mcp_to_fstar_sessions = self.mcp_to_fstar_sessions.clone();
            let timed_session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                if sessions.write().await.remove(&session_id).is_none() {
                    return;
                }
                file_to_session
                    .write()
                    .await
                    .retain(|_, id| id != &session_id);
                if let Some(owner) = &timed_session.mcp_session_id {
                    let mut owners = mcp_to_fstar_sessions.write().await;
                    if let Some(ids) = owners.get_mut(owner) {
                        ids.remove(&session_id);
                        if ids.is_empty() {
                            owners.remove(owner);
                        }
                    }
                }
                timed_out.write().await.insert(session_id.clone(), seconds);
                timed_session.kill().await;
            });
        }
        Ok(session)
    }

    async fn evict_lru_if_needed(&self) {
        while self.sessions.read().await.len() >= self.max_sessions {
            let sessions: Vec<Arc<Session>> =
                self.sessions.read().await.values().cloned().collect();
            let mut oldest: Option<(String, DateTime<Utc>)> = None;
            for session in sessions {
                let Ok(checker) = session.checker.try_lock() else {
                    continue;
                };
                drop(checker);
                let last_activity = session.state.read().await.last_activity;
                if oldest
                    .as_ref()
                    .map(|(_, current)| last_activity < *current)
                    .unwrap_or(true)
                {
                    oldest = Some((session.id.clone(), last_activity));
                }
            }
            let Some((session_id, _)) = oldest else {
                break;
            };
            tracing::info!(%session_id, "Evicting least recently used F* session");
            if let Some(session) = self.sessions.read().await.get(&session_id).cloned() {
                self.evicted_keys.write().await.insert((
                    Self::owner_key(session.mcp_session_id.as_deref()),
                    session.file_path.clone(),
                    config_fingerprint(&session.config),
                ));
            }
            let _ = self.close_session(&session_id, None, true).await;
        }
    }

    pub async fn list_sessions(&self, mcp_session_id: Option<&str>) -> Vec<SessionInfo> {
        let sessions: Vec<Arc<Session>> = self
            .sessions
            .read()
            .await
            .values()
            .filter(|session| session.mcp_session_id.as_deref() == mcp_session_id)
            .cloned()
            .collect();
        let now = Utc::now();
        let mut result = Vec::with_capacity(sessions.len());
        for session in sessions {
            let state = session.state.read().await;
            if !state.marked_for_deletion {
                result.push(SessionInfo {
                    session_id: session.id.clone(),
                    file_path: session.file_path.to_string_lossy().into_owned(),
                    created_at: session.created_at.to_rfc3339(),
                    last_activity: state.last_activity.to_rfc3339(),
                    idle_seconds: (now - state.last_activity).num_seconds(),
                    mcp_session_id: session.mcp_session_id.clone(),
                    marked_for_deletion: state.marked_for_deletion,
                });
            }
        }
        result
    }

    pub async fn mark_sessions_for_deletion(&self, mcp_session_id: &str) {
        let ids = self
            .mcp_to_fstar_sessions
            .read()
            .await
            .get(mcp_session_id)
            .cloned()
            .unwrap_or_default();
        let sessions = self.sessions.read().await;
        for id in ids {
            if let Some(session) = sessions.get(&id) {
                session.state.write().await.marked_for_deletion = true;
            }
        }
    }

    pub async fn sweep_marked_sessions(&self) -> usize {
        let sessions: Vec<Arc<Session>> = self.sessions.read().await.values().cloned().collect();
        let mut ids = Vec::new();
        let now = Utc::now();
        for session in sessions {
            let state = session.state.read().await;
            let idle_seconds = (now - state.last_activity).num_seconds().max(0) as u64;
            if state.marked_for_deletion || idle_seconds >= self.idle_timeout_secs {
                if !state.marked_for_deletion {
                    self.evicted_keys.write().await.insert((
                        Self::owner_key(session.mcp_session_id.as_deref()),
                        session.file_path.clone(),
                        config_fingerprint(&session.config),
                    ));
                }
                ids.push(session.id.clone());
            }
        }
        for id in &ids {
            let _ = self.close_session(id, None, true).await;
        }
        ids.len()
    }

    pub async fn close_session(
        &self,
        session_id: &str,
        mcp_session_id: Option<&str>,
        force: bool,
    ) -> Result<(), SessionError> {
        let session = self
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if !force && session.mcp_session_id.as_deref() != mcp_session_id {
            return Err(SessionError::NotOwned {
                session_id: session_id.to_string(),
            });
        }
        self.sessions.write().await.remove(session_id);
        self.file_to_session
            .write()
            .await
            .retain(|_, id| id != session_id);
        if let Some(owner) = &session.mcp_session_id {
            let mut owners = self.mcp_to_fstar_sessions.write().await;
            if let Some(ids) = owners.get_mut(owner) {
                ids.remove(session_id);
                if ids.is_empty() {
                    owners.remove(owner);
                }
            }
        }
        session.kill().await;
        Ok(())
    }

    pub async fn close_all(&self) {
        let session_ids: Vec<String> = self.sessions.read().await.keys().cloned().collect();
        for session_id in session_ids {
            let _ = self.close_session(&session_id, None, true).await;
        }
    }
}

pub fn content_hash(contents: &str) -> String {
    format!("{:x}", Sha256::digest(contents.as_bytes()))
}

fn config_fingerprint(config: &FStarConfig) -> String {
    content_hash(&serde_json::to_string(config).unwrap_or_default())
}

async fn canonical_path(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

async fn hash_file(path: &Path) -> Option<String> {
    tokio::fs::read(path)
        .await
        .ok()
        .map(|contents| format!("{:x}", Sha256::digest(contents)))
}

async fn hash_files(paths: &[PathBuf]) -> HashMap<PathBuf, String> {
    let mut hashes = HashMap::new();
    for path in paths {
        if let Some(hash) = hash_file(path).await {
            hashes.insert(path.clone(), hash);
        }
    }
    hashes
}

fn changed_dependencies(
    previous: &HashMap<PathBuf, String>,
    current: &HashMap<PathBuf, String>,
) -> Vec<String> {
    current
        .iter()
        .filter(|(path, hash)| previous.get(*path) != Some(*hash))
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .chain(
            previous
                .keys()
                .filter(|path| !current.contains_key(*path))
                .map(|path| path.to_string_lossy().into_owned()),
        )
        .collect()
}

async fn discover_dependencies(
    file_path: &Path,
    config: &FStarConfig,
    code: &str,
    mut dependencies: Vec<PathBuf>,
    run_fstar_dep: bool,
) -> Vec<PathBuf> {
    let cwd = config.cwd_or(file_path.parent().unwrap_or(Path::new(".")));
    let mut roots = vec![cwd.clone()];
    roots.extend(config.include_dirs.iter().map(|directory| {
        let directory = PathBuf::from(directory);
        if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        }
    }));
    let mut modules = Vec::new();
    for line in code.lines() {
        let line = line.trim_start();
        let module = line
            .strip_prefix("open ")
            .or_else(|| line.strip_prefix("include "))
            .and_then(|rest| rest.split_whitespace().next());
        if let Some(module) = module {
            modules.push(module.trim_end_matches(';').to_string());
        }
    }

    if run_fstar_dep {
        dependencies.extend(fstar_dependencies(file_path, config).await);
    }
    for module in modules {
        let relative = module.replace('.', "/");
        for root in &roots {
            for extension in ["fst", "fsti"] {
                let candidate = root.join(format!("{relative}.{extension}"));
                if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
                    dependencies.push(canonical_path(&candidate).await);
                    break;
                }
            }
        }
    }
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

async fn fstar_dependencies(file_path: &Path, config: &FStarConfig) -> Vec<PathBuf> {
    let cwd = config.cwd_or(file_path.parent().unwrap_or(Path::new(".")));
    let mut arguments = vec!["--dep".to_string(), "full".to_string()];
    arguments.extend(config.options.clone());
    for include in &config.include_dirs {
        arguments.push("--include".to_string());
        arguments.push(include.clone());
    }
    arguments.push(file_path.to_string_lossy().into_owned());

    let output = match tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(config.fstar_exe())
            .args(arguments)
            .current_dir(&cwd)
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => output,
        _ => return Vec::new(),
    };
    let current = canonical_path(file_path).await;
    let mut dependencies = Vec::new();
    for token in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        let token = token.trim_matches(|character: char| {
            matches!(character, ':' | ';' | ',' | '(' | ')' | '[' | ']' | '\\')
        });
        if !(token.ends_with(".fst") || token.ends_with(".fsti")) {
            continue;
        }
        let path = PathBuf::from(token);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let path = canonical_path(&path).await;
        if path != current {
            dependencies.push(path);
        }
    }
    dependencies
}

fn first_diff_position(previous: &str, current: &str) -> Option<(u32, u32)> {
    let mut previous_chars = previous.char_indices();
    let mut current_chars = current.char_indices();
    let offset = loop {
        match (previous_chars.next(), current_chars.next()) {
            (Some((_, left)), Some((offset, right))) if left != right => break Some(offset),
            (Some(_), Some(_)) => {}
            (None, Some((offset, _))) => break Some(offset),
            (Some(_), None) => break Some(current.len()),
            (None, None) => break None,
        }
    }?;
    let prefix = &current[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1;
    let column = prefix
        .rsplit_once('\n')
        .map(|(_, suffix)| suffix.chars().count() as u32)
        .unwrap_or_else(|| prefix.chars().count() as u32);
    Some((line, column))
}

fn count_reused_prefix(previous: &[FragmentResult], current: &[FragmentResult]) -> usize {
    previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left.range == right.range && left.status == right.status)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_first_diff_position() {
        assert_eq!(first_diff_position("a\nbc", "a\nbd"), Some((2, 1)));
        assert_eq!(first_diff_position("same", "same"), None);
        assert_eq!(first_diff_position("short", "shorter"), Some((1, 5)));
        assert_eq!(first_diff_position("é", "ê"), Some((1, 0)));
    }

    #[test]
    fn hashes_are_stable() {
        assert_eq!(content_hash("same"), content_hash("same"));
        assert_ne!(content_hash("same"), content_hash("different"));
    }
}
