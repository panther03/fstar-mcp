//! F* IDE protocol handling (JSON-L over stdio).

use crate::fstar::messages::*;
use crate::is_verbose;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

/// JSON-L interface for communicating with F* IDE process
#[derive(Clone)]
pub struct JsonlInterface {
    writer: Arc<Mutex<ChildStdin>>,
}

impl JsonlInterface {
    pub fn new(stdin: ChildStdin) -> Self {
        Self {
            writer: Arc::new(Mutex::new(stdin)),
        }
    }

    /// Send a JSON message (adds newline)
    pub async fn send_message(&self, msg: &serde_json::Value) -> std::io::Result<()> {
        let mut writer = self.writer.lock().await;
        let json = serde_json::to_string(msg)?;

        if is_verbose() {
            tracing::info!("[F* ← MCP] {}", json);
        }

        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Response from F* IDE process
#[derive(Debug, Clone)]
pub enum FStarResponse {
    /// Protocol info (sent on startup)
    ProtocolInfo(ProtocolInfo),
    /// Regular response to a query
    Response(IdeResponseBase),
    /// Progress message (during full-buffer)
    Progress {
        query_id: String,
        stage: String,
        ranges: Option<FStarRange>,
    },
    /// Proof state (from tactics)
    ProofState {
        query_id: String,
        proof_state: IdeProofState,
    },
    /// Error/warning/info message
    StatusMessage {
        #[allow(dead_code)]
        query_id: String,
        level: String,
        contents: String,
    },
}

/// Parse a line from F* IDE output into a structured response
pub fn parse_response(line: &str) -> Result<FStarResponse, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(line)?;

    // Check for protocol-info
    if ProtocolInfo::is_protocol_info(&value) {
        let info: ProtocolInfo = serde_json::from_value(value)?;
        return Ok(FStarResponse::ProtocolInfo(info));
    }

    let kind = value.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let query_id = value
        .get("query-id")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .to_string();

    match kind {
        "response" => {
            let response: IdeResponseBase = serde_json::from_value(value)?;
            Ok(FStarResponse::Response(response))
        }
        "message" => {
            let level = value
                .get("level")
                .and_then(|l| l.as_str())
                .unwrap_or("")
                .to_string();

            match level.as_str() {
                "progress" => {
                    let contents = value.get("contents").cloned().unwrap_or_default();
                    let stage = contents
                        .get("stage")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let ranges = contents
                        .get("ranges")
                        .and_then(|r| serde_json::from_value(r.clone()).ok());

                    Ok(FStarResponse::Progress {
                        query_id,
                        stage,
                        ranges,
                    })
                }
                "proof-state" => {
                    let contents = value.get("contents").cloned().unwrap_or_default();
                    let proof_state: IdeProofState = serde_json::from_value(contents)?;
                    Ok(FStarResponse::ProofState {
                        query_id,
                        proof_state,
                    })
                }
                "error" | "warning" | "info" => {
                    let contents = value
                        .get("contents")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    Ok(FStarResponse::StatusMessage {
                        query_id,
                        level,
                        contents,
                    })
                }
                _ => {
                    // Unknown message type, return as generic response
                    let response: IdeResponseBase = serde_json::from_value(value)?;
                    Ok(FStarResponse::Response(response))
                }
            }
        }
        _ => {
            // Unknown kind, try to parse as response
            let response: IdeResponseBase = serde_json::from_value(value)?;
            Ok(FStarResponse::Response(response))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_protocol_info() {
        let line = r#"{"kind":"protocol-info","version":3,"features":["full-buffer","vfs-add"]}"#;
        let response = parse_response(line).unwrap();
        match response {
            FStarResponse::ProtocolInfo(info) => {
                assert_eq!(info.version, 3);
                assert!(info.supports_full_buffer());
            }
            _ => panic!("Expected ProtocolInfo"),
        }
    }

    #[test]
    fn test_parse_progress() {
        let line = r#"{"query-id":"1","kind":"message","level":"progress","contents":{"stage":"full-buffer-started"}}"#;
        let response = parse_response(line).unwrap();
        match response {
            FStarResponse::Progress {
                query_id, stage, ..
            } => {
                assert_eq!(query_id, "1");
                assert_eq!(stage, "full-buffer-started");
            }
            _ => panic!("Expected Progress"),
        }
    }

    #[test]
    fn test_parse_response_success() {
        let line = r#"{"query-id":"1","kind":"response","status":"success","response":null}"#;
        let response = parse_response(line).unwrap();
        match response {
            FStarResponse::Response(r) => {
                assert_eq!(r.query_id, "1");
                assert_eq!(r.status, Some("success".to_string()));
            }
            _ => panic!("Expected Response"),
        }
    }
}
