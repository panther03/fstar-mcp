//! MCP tool implementations for F* IDE.

use crate::fstar::{FStarConfig, IdeLookupResponse};
use crate::session::{
    content_hash, DiagnosticInfo, FragmentInfo, LookupResponse, RangeInfo, Session, SessionError,
    SessionManager, TypecheckResponse, DEFAULT_QUERY_TIMEOUT_SECS,
};
use async_trait::async_trait;
use pmcp::types::capabilities::ServerCapabilities;
use pmcp::types::ToolInfo;
use pmcp::{Server, ToolHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{collections::HashMap, fs};

lazy_static::lazy_static! {
    pub static ref SESSION_MANAGER: Arc<SessionManager> = Arc::new(SessionManager::new());
}

fn tool_error(error: impl std::fmt::Display) -> pmcp::Error {
    pmcp::Error::validation(error.to_string())
}

async fn session_by_id(session_id: &str, owner: Option<&str>) -> pmcp::Result<Arc<Session>> {
    match SESSION_MANAGER.get_owned(session_id, owner).await {
        Ok(session) => Ok(session),
        Err(SessionError::NotFound(_)) => {
            if let Some(seconds) = SESSION_MANAGER.get_timeout_info(session_id).await {
                Err(tool_error(format!(
                    "Session timed out after {seconds} seconds: {session_id}"
                )))
            } else {
                Err(tool_error(format!("Session not found: {session_id}")))
            }
        }
        Err(error) => Err(tool_error(error)),
    }
}

async fn session_for_path(
    file_path: &Path,
    owner: Option<String>,
    workspace_root: Option<&Path>,
) -> pmcp::Result<Arc<Session>> {
    let config = FStarConfig::discover(file_path, workspace_root).map_err(tool_error)?;
    if let Some(session) = SESSION_MANAGER
        .find_by_path(file_path, owner.as_deref(), &config)
        .await
    {
        return Ok(session);
    }
    SESSION_MANAGER
        .create_session(file_path, config, owner, None)
        .await
        .map_err(tool_error)
}

pub struct CreateSessionTool;

#[derive(Debug, Deserialize)]
struct CreateSessionArgs {
    file_path: Option<String>,
    fstar_exe: Option<String>,
    cwd: Option<String>,
    include_dirs: Option<Vec<String>>,
    options: Option<Vec<String>>,
    timeout: Option<u64>,
    workspace_root: Option<String>,
}

#[async_trait]
impl ToolHandler for CreateSessionTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: CreateSessionArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let (file_path, created_file) = if let Some(path) = params.file_path {
            (PathBuf::from(path), false)
        } else {
            let filename = format!("fstar_session_{}.fst", uuid::Uuid::new_v4());
            let path = std::env::temp_dir().join(&filename);
            let module_name = filename.replace(".fst", "").replace('-', "_");
            tokio::fs::write(&path, format!("module {module_name}\n"))
                .await
                .map_err(tool_error)?;
            (path, true)
        };
        let overrides = FStarConfig {
            fstar_exe: params.fstar_exe,
            cwd: params.cwd,
            include_dirs: params.include_dirs.unwrap_or_default(),
            options: params.options.unwrap_or_default(),
        };
        let config = FStarConfig::discover_with_overrides(
            &file_path,
            params.workspace_root.as_deref().map(Path::new),
            &overrides,
        )
        .map_err(tool_error)?;
        let session = SESSION_MANAGER
            .create_session(&file_path, config, extra.session_id.clone(), params.timeout)
            .await
            .map_err(tool_error)?;

        Ok(json!({
            "session_id": session.id,
            "file_path": session.file_path,
            "status": "ready",
            "created_file": created_file,
            "created_at": session.created_at.to_rfc3339(),
            "message": "F* checker and lax companion are warm; call typecheck_buffer after editing the file."
        }))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "create_session",
            Some(
                "Warm an F* checker and lax companion without verifying the file. Usually unnecessary because path-based tools create sessions automatically."
                    .to_string(),
            ),
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "fstar_exe": {"type": "string"},
                    "cwd": {"type": "string"},
                    "include_dirs": {"type": "array", "items": {"type": "string"}},
                    "options": {"type": "array", "items": {"type": "string"}},
                    "timeout": {"type": "integer", "minimum": 1},
                    "workspace_root": {"type": "string"}
                }
            }),
        ))
    }
}

pub struct TypecheckBufferTool;

#[derive(Debug, Deserialize)]
struct TypecheckBufferArgs {
    session_id: Option<String>,
    file_path: Option<String>,
    #[serde(alias = "code")]
    content: Option<String>,
    kind: Option<String>,
    lax: Option<bool>,
    to_line: Option<u32>,
    to_column: Option<u32>,
    timeout: Option<u64>,
    workspace_root: Option<String>,
}

#[async_trait]
impl ToolHandler for TypecheckBufferTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: TypecheckBufferArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let owner = extra.session_id.clone();
        let session = match (&params.session_id, &params.file_path) {
            (Some(id), _) => session_by_id(id, owner.as_deref()).await?,
            (None, Some(path)) => {
                session_for_path(
                    Path::new(path),
                    owner,
                    params.workspace_root.as_deref().map(Path::new),
                )
                .await?
            }
            (None, None) => {
                return Err(tool_error("Provide file_path (preferred) or session_id"));
            }
        };
        if params.session_id.is_some() {
            if let Some(path) = &params.file_path {
                let supplied = tokio::fs::canonicalize(path).await.map_err(tool_error)?;
                if supplied != session.file_path {
                    return Err(tool_error(format!(
                        "Session {} is for {}, not {}",
                        session.id,
                        session.file_path.display(),
                        supplied.display()
                    )));
                }
            }
        }
        let file_path = if params.session_id.is_some() {
            session.file_path.clone()
        } else {
            PathBuf::from(params.file_path.as_deref().expect("validated above"))
        };
        let (code, disk_hash) = if let Some(content) = params.content {
            (content, None)
        } else {
            let code = tokio::fs::read_to_string(&file_path)
                .await
                .map_err(|error| {
                    tool_error(format!("Failed to read {}: {error}", file_path.display()))
                })?;
            let hash = content_hash(&code);
            (code, Some(hash))
        };
        let kind = if params.lax.unwrap_or(false) {
            "lax"
        } else {
            params.kind.as_deref().unwrap_or("full")
        };
        let to_position = params.to_line.zip(params.to_column);
        let timeout = Duration::from_secs(params.timeout.unwrap_or(DEFAULT_QUERY_TIMEOUT_SECS));
        let started = Instant::now();
        let check = session.check(&code, kind, to_position, timeout, disk_hash.as_deref());
        tokio::pin!(check);
        let checked = tokio::select! {
            result = &mut check => result.map_err(tool_error)?,
            _ = extra.cancelled() => {
                session
                    .cancel_current(
                        matches!(kind, "lax" | "lax-to-position"),
                        (1, 0),
                    )
                    .await
                    .map_err(tool_error)?;
                return Err(pmcp::Error::cancelled());
            }
        };
        let duration_ms = started.elapsed().as_millis();
        let has_errors = checked
            .result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.level == "error");
        let status = if checked.stale {
            "stale"
        } else if checked.result.timed_out {
            "partial"
        } else if has_errors {
            "error"
        } else {
            "ok"
        };
        let total_fragments = checked.result.fragments.len();
        let summary = format!(
            "{status}: verified through line {}; reused {}/{} fragments; {} ms{}",
            checked.verified_through_line,
            checked.reused_fragments,
            total_fragments,
            duration_ms,
            if checked.process_restarted {
                "; F* restarted and cache was lost"
            } else {
                ""
            }
        );
        let diagnostics = checked
            .result
            .diagnostics
            .iter()
            .take(20)
            .map(DiagnosticInfo::from)
            .collect();
        Ok(serde_json::to_value(TypecheckResponse {
            status: status.to_string(),
            summary,
            file_path: file_path.to_string_lossy().into_owned(),
            content_hash: checked.content_hash,
            stale: checked.stale,
            timed_out: checked.result.timed_out,
            finished: checked.result.finished,
            reused_fragments: checked.reused_fragments,
            total_fragments,
            verified_through_line: checked.verified_through_line,
            duration_ms,
            process_restarted: checked.process_restarted,
            dependencies_changed: checked.dependencies_changed,
            hint: checked.hint,
            diagnostics,
        })?)
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "typecheck_buffer",
            Some(
                "Check an F* file from disk using an implicit path-keyed warm session. Use content only for an unsaved buffer."
                    .to_string(),
            ),
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "F* file to read and check (preferred)."},
                    "session_id": {"type": "string", "description": "Legacy explicit session ID."},
                    "content": {"type": "string", "description": "Optional unsaved-buffer override; disk is the default source of truth."},
                    "lax": {"type": "boolean"},
                    "kind": {"type": "string", "enum": ["full", "lax", "cache", "reload-deps", "verify-to-position", "lax-to-position"]},
                    "to_line": {"type": "integer"},
                    "to_column": {"type": "integer"},
                    "timeout": {"type": "integer", "minimum": 1, "default": DEFAULT_QUERY_TIMEOUT_SECS}
                    ,"workspace_root": {"type": "string", "description": "Stop config discovery at this directory."}
                },
                "anyOf": [{"required": ["file_path"]}, {"required": ["session_id"]}]
            }),
        ))
    }
}

pub struct UpdateBufferTool;

#[derive(Debug, Deserialize)]
struct UpdateBufferArgs {
    session_id: String,
    file_path: String,
    contents: String,
}

#[async_trait]
impl ToolHandler for UpdateBufferTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: UpdateBufferArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let session = session_by_id(&params.session_id, extra.session_id.as_deref()).await?;
        session
            .vfs_add(Some(&params.file_path), &params.contents)
            .await
            .map_err(tool_error)?;
        Ok(json!({"status": "ok"}))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "update_buffer",
            Some("Add or update a file in both F* virtual file systems.".to_string()),
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "file_path": {"type": "string"},
                    "contents": {"type": "string"}
                },
                "required": ["session_id", "file_path", "contents"]
            }),
        ))
    }
}

pub struct LookupSymbolTool;

#[derive(Debug, Deserialize)]
struct LookupSymbolArgs {
    session_id: Option<String>,
    file_path: String,
    line: u32,
    column: u32,
    symbol: String,
}

#[async_trait]
impl ToolHandler for LookupSymbolTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: LookupSymbolArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let session = if let Some(id) = &params.session_id {
            session_by_id(id, extra.session_id.as_deref()).await?
        } else {
            session_for_path(Path::new(&params.file_path), extra.session_id.clone(), None).await?
        };
        let result = session
            .lookup(
                &params.file_path,
                params.line,
                params.column,
                &params.symbol,
            )
            .await
            .map_err(tool_error)?;
        let response = match result {
            Some(IdeLookupResponse::Symbol(symbol)) => LookupResponse {
                kind: "symbol".to_string(),
                name: Some(symbol.name),
                type_info: symbol.type_info,
                documentation: symbol.documentation,
                defined_at: symbol.defined_at.as_ref().map(RangeInfo::from),
            },
            Some(IdeLookupResponse::Module(module)) => LookupResponse {
                kind: "module".to_string(),
                name: Some(module.name),
                type_info: None,
                documentation: None,
                defined_at: Some(RangeInfo {
                    file: module.path,
                    start_line: 1,
                    start_column: 0,
                    end_line: 1,
                    end_column: 0,
                }),
            },
            None => LookupResponse {
                kind: "not_found".to_string(),
                name: None,
                type_info: None,
                documentation: None,
                defined_at: None,
            },
        };
        Ok(serde_json::to_value(response)?)
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "lookup_symbol",
            Some("Look up a symbol through the non-blocking lax companion process.".to_string()),
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "file_path": {"type": "string"},
                    "line": {"type": "integer"},
                    "column": {"type": "integer"},
                    "symbol": {"type": "string"}
                },
                "required": ["file_path", "line", "column", "symbol"]
            }),
        ))
    }
}

pub struct RestartSolverTool;

#[derive(Debug, Deserialize)]
struct RestartSolverArgs {
    session_id: Option<String>,
    file_path: Option<String>,
}

#[async_trait]
impl ToolHandler for RestartSolverTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: RestartSolverArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let session = match (params.session_id, params.file_path) {
            (Some(id), _) => session_by_id(&id, extra.session_id.as_deref()).await?,
            (None, Some(path)) => {
                session_for_path(Path::new(&path), extra.session_id.clone(), None).await?
            }
            _ => return Err(tool_error("Provide file_path or session_id")),
        };
        session.restart_solver().await.map_err(tool_error)?;
        Ok(json!({"status": "ok"}))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "restart_solver",
            Some("Terminate Z3 descendants and restart both session solvers.".to_string()),
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "file_path": {"type": "string"}
                },
                "anyOf": [{"required": ["file_path"]}, {"required": ["session_id"]}]
            }),
        ))
    }
}

pub struct CloseSessionTool;

#[derive(Debug, Deserialize)]
struct CloseSessionArgs {
    session_id: String,
}

#[async_trait]
impl ToolHandler for CloseSessionTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: CloseSessionArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        SESSION_MANAGER
            .close_session(&params.session_id, extra.session_id.as_deref(), false)
            .await
            .map_err(tool_error)?;
        Ok(json!({"status": "ok"}))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "close_session",
            Some("Close an explicitly identified session owned by this MCP client.".to_string()),
            json!({
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }),
        ))
    }
}

pub struct ListSessionsTool;

#[async_trait]
impl ToolHandler for ListSessionsTool {
    async fn handle(&self, _args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let sessions = SESSION_MANAGER
            .list_sessions(extra.session_id.as_deref())
            .await;
        Ok(json!({"count": sessions.len(), "sessions": sessions}))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "list_sessions",
            Some("List sessions owned by this MCP client.".to_string()),
            json!({"type": "object", "properties": {}}),
        ))
    }
}

pub struct GetProofContextTool;

#[derive(Debug, Deserialize)]
struct GetProofContextInput {
    session_id: Option<String>,
    file_path: Option<String>,
    line: Option<u32>,
}

#[async_trait]
impl ToolHandler for GetProofContextTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: GetProofContextInput =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let session = match (params.session_id, params.file_path) {
            (Some(id), _) => session_by_id(&id, extra.session_id.as_deref()).await?,
            (None, Some(path)) => {
                session_for_path(Path::new(&path), extra.session_id.clone(), None).await?
            }
            _ => return Err(tool_error("Provide file_path or session_id")),
        };
        let states = session.proof_states().await;
        if let Some(line) = params.line {
            let state = states
                .into_iter()
                .find(|state| state.location.beg.0 == line);
            Ok(json!({"found": state.is_some(), "line": line, "proof_state": state}))
        } else {
            Ok(json!({"count": states.len(), "proof_states": states}))
        }
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "get_proof_context",
            Some("Get proof states collected by the latest check.".to_string()),
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "file_path": {"type": "string"},
                    "line": {"type": "integer"}
                },
                "anyOf": [{"required": ["file_path"]}, {"required": ["session_id"]}]
            }),
        ))
    }
}

pub struct GetStatusTool;

#[derive(Debug, Deserialize)]
struct GetStatusArgs {
    session_id: Option<String>,
    file_path: Option<String>,
}

#[async_trait]
impl ToolHandler for GetStatusTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: GetStatusArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let session = match (params.session_id, params.file_path) {
            (Some(id), _) => session_by_id(&id, extra.session_id.as_deref()).await?,
            (None, Some(path)) => {
                session_for_path(Path::new(&path), extra.session_id.clone(), None).await?
            }
            _ => return Err(tool_error("Provide file_path or session_id")),
        };
        let fragments: Vec<FragmentInfo> = session
            .fragments()
            .await
            .iter()
            .map(FragmentInfo::from)
            .collect();
        Ok(json!({
            "file_path": session.file_path,
            "fragment_count": fragments.len(),
            "fragments": fragments
        }))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "get_status",
            Some("Get detailed fragment ranges from the latest check.".to_string()),
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "file_path": {"type": "string"}
                },
                "anyOf": [{"required": ["file_path"]}, {"required": ["session_id"]}]
            }),
        ))
    }
}

pub struct CheckProjectTool;

#[derive(Debug, Deserialize)]
struct CheckProjectArgs {
    workspace_root: String,
    files: Option<Vec<String>>,
    timeout_per_file: Option<u64>,
}

#[async_trait]
impl ToolHandler for CheckProjectTool {
    async fn handle(&self, args: Value, extra: pmcp::RequestHandlerExtra) -> pmcp::Result<Value> {
        let params: CheckProjectArgs =
            serde_json::from_value(args).map_err(|error| tool_error(error))?;
        let root = PathBuf::from(&params.workspace_root);
        let files = if let Some(files) = params.files {
            files
                .into_iter()
                .map(|file| {
                    let path = PathBuf::from(file);
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
                .collect()
        } else {
            collect_fstar_files(&root).map_err(tool_error)?
        };
        if files.is_empty() {
            return Err(tool_error("No .fst or .fsti files found"));
        }
        let ordered = dependency_order(files).map_err(tool_error)?;
        let timeout = Duration::from_secs(
            params
                .timeout_per_file
                .unwrap_or(DEFAULT_QUERY_TIMEOUT_SECS),
        );
        let mut results = Vec::new();
        let mut failures = 0;
        for file_path in ordered {
            let code = tokio::fs::read_to_string(&file_path)
                .await
                .map_err(tool_error)?;
            let hash = content_hash(&code);
            let session =
                session_for_path(&file_path, extra.session_id.clone(), Some(root.as_path()))
                    .await?;
            let started = Instant::now();
            let check = session.check(&code, "full", None, timeout, Some(&hash));
            tokio::pin!(check);
            let checked = tokio::select! {
                result = &mut check => result.map_err(tool_error)?,
                _ = extra.cancelled() => {
                    session.cancel_current(false, (1, 0)).await.map_err(tool_error)?;
                    return Err(pmcp::Error::cancelled());
                }
            };
            let has_errors = checked
                .result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.level == "error");
            if has_errors || checked.result.timed_out || checked.stale {
                failures += 1;
            }
            results.push(json!({
                "file_path": file_path,
                "status": if checked.stale {
                    "stale"
                } else if checked.result.timed_out {
                    "partial"
                } else if has_errors {
                    "error"
                } else {
                    "ok"
                },
                "verified_through_line": checked.verified_through_line,
                "reused_fragments": checked.reused_fragments,
                "total_fragments": checked.result.fragments.len(),
                "duration_ms": started.elapsed().as_millis(),
                "diagnostics": checked.result.diagnostics.iter().take(10).map(DiagnosticInfo::from).collect::<Vec<_>>()
            }));
        }
        Ok(json!({
            "status": if failures == 0 { "ok" } else { "error" },
            "checked_files": results.len(),
            "failed_files": failures,
            "results": results,
            "note": "Files were checked in source dependency order. Run the project build when durable .checked artifacts are required."
        }))
    }

    fn metadata(&self) -> Option<ToolInfo> {
        Some(ToolInfo::new(
            "check_project",
            Some(
                "Check F* files in dependency order, reloading changed dependencies in warm sessions."
                    .to_string(),
            ),
            json!({
                "type": "object",
                "properties": {
                    "workspace_root": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}},
                    "timeout_per_file": {"type": "integer", "minimum": 1}
                },
                "required": ["workspace_root"]
            }),
        ))
    }
}

fn collect_fstar_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".git" | "target" | "_build")
                ) {
                    directories.push(path);
                }
            } else if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("fst" | "fsti")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn dependency_order(files: Vec<PathBuf>) -> std::io::Result<Vec<PathBuf>> {
    let mut modules = HashMap::new();
    let mut sources = HashMap::new();
    for file in &files {
        let source = fs::read_to_string(file)?;
        let module = source.lines().find_map(|line| {
            line.trim_start()
                .strip_prefix("module ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|name| name.trim_end_matches(';').to_string())
        });
        if let Some(module) = module {
            modules.insert(module, file.clone());
        }
        sources.insert(file.clone(), source);
    }
    let mut dependencies: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for file in &files {
        let mut file_dependencies = Vec::new();
        for line in sources[file].lines() {
            let line = line.trim_start();
            let module = line
                .strip_prefix("open ")
                .or_else(|| line.strip_prefix("include "))
                .and_then(|rest| rest.split_whitespace().next())
                .map(|name| name.trim_end_matches(';'));
            if let Some(dependency) = module.and_then(|module| modules.get(module)) {
                if dependency != file {
                    file_dependencies.push(dependency.clone());
                }
            }
        }
        file_dependencies.sort();
        file_dependencies.dedup();
        dependencies.insert(file.clone(), file_dependencies);
    }

    let mut remaining = files;
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .position(|file| dependencies[file].iter().all(|dep| ordered.contains(dep)));
        let index = ready.unwrap_or(0);
        ordered.push(remaining.remove(index));
    }
    Ok(ordered)
}

pub fn create_fstar_server() -> Result<Server, Box<dyn std::error::Error>> {
    Ok(Server::builder()
        .name("fstar-mcp")
        .version(env!("CARGO_PKG_VERSION"))
        .capabilities(ServerCapabilities::tools_only())
        .tool("typecheck_buffer", TypecheckBufferTool)
        .tool("lookup_symbol", LookupSymbolTool)
        .tool("get_proof_context", GetProofContextTool)
        .tool("get_status", GetStatusTool)
        .tool("check_project", CheckProjectTool)
        .tool("restart_solver", RestartSolverTool)
        .tool("create_session", CreateSessionTool)
        .tool("update_buffer", UpdateBufferTool)
        .tool("close_session", CloseSessionTool)
        .tool("list_sessions", ListSessionsTool)
        .build()?)
}
