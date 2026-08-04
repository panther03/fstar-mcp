//! Stdio transport that survives pre-initialize discovery probes.
//!
//! MCP revision `2026-07-28` lets a client open the lifecycle with a
//! `server/discover` request before `initialize`. The `pmcp` SDK only speaks up
//! to `2025-11-25`, and its stdio transport treats any unparseable frame as a
//! fatal receive error, so the process exits without replying and the client
//! reports `failed to negotiate MCP lifecycle: connection closed`.
//!
//! JSON-RPC already defines the correct answer for a method a server does not
//! implement: `-32601 Method not found`. Replying with that lets a
//! `2026-07-28` client fall back to the classic `initialize` handshake that the
//! SDK does support, instead of dropping the connection.

use async_trait::async_trait;
use pmcp::error::TransportError;
use pmcp::shared::transport::{parse_message, serialize_message};
use pmcp::shared::{Transport, TransportMessage};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const METHOD_NOT_FOUND: i32 = -32601;

/// Stdio transport that answers unknown pre-initialize requests with a
/// JSON-RPC error rather than closing the connection.
#[derive(Debug)]
pub struct DiscoveryAwareStdioTransport {
    stdin: Mutex<BufReader<tokio::io::Stdin>>,
    /// Persistent partial-line buffer keeping `receive()` cancel-safe.
    ///
    /// `read_until` appends straight into this buffer, so a receive future that
    /// is dropped mid-line retains the bytes it already consumed.
    partial: Mutex<Vec<u8>>,
    stdout: Mutex<tokio::io::Stdout>,
    closed: AtomicBool,
}

impl DiscoveryAwareStdioTransport {
    pub fn new() -> Self {
        Self {
            stdin: Mutex::new(BufReader::new(tokio::io::stdin())),
            partial: Mutex::new(Vec::new()),
            stdout: Mutex::new(tokio::io::stdout()),
            closed: AtomicBool::new(false),
        }
    }

    async fn write_line(&self, bytes: &[u8]) -> Result<(), pmcp::Error> {
        let mut stdout = self.stdout.lock().await;
        stdout.write_all(bytes).await.map_err(TransportError::from)?;
        stdout.write_all(b"\n").await.map_err(TransportError::from)?;
        stdout.flush().await.map_err(TransportError::from)?;
        Ok(())
    }

    async fn read_line(&self) -> Result<Option<Vec<u8>>, pmcp::Error> {
        // Hold both guards for the whole read so the reader and the persistent
        // buffer advance atomically.
        let mut stdin = self.stdin.lock().await;
        let mut partial = self.partial.lock().await;

        loop {
            if let Some(idx) = partial.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = partial.drain(..=idx).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(line));
            }

            let bytes_read = stdin
                .read_until(b'\n', &mut partial)
                .await
                .map_err(TransportError::from)?;
            if bytes_read == 0 {
                self.closed.store(true, Ordering::Release);
                return Ok(None);
            }
        }
    }
}

impl Default for DiscoveryAwareStdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `-32601` reply for a request the SDK cannot handle.
///
/// Returns `None` when the frame is not a request that requires a reply, so
/// notifications are never answered (JSON-RPC forbids replying to them).
pub fn method_not_found_response(line: &[u8]) -> Option<Vec<u8>> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let method = value.get("method")?.as_str()?;
    let id = value.get("id")?;
    if id.is_null() {
        return None;
    }

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": METHOD_NOT_FOUND,
            "message": format!("Method not found: {method}"),
        }
    });
    serde_json::to_vec(&response).ok()
}

#[async_trait]
impl Transport for DiscoveryAwareStdioTransport {
    async fn send(&mut self, message: TransportMessage) -> Result<(), pmcp::Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::ConnectionClosed.into());
        }
        let bytes = serialize_message(&message)?;
        self.write_line(&bytes).await
    }

    async fn receive(&mut self) -> Result<TransportMessage, pmcp::Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::ConnectionClosed.into());
        }

        loop {
            let Some(line) = self.read_line().await? else {
                return Err(TransportError::ConnectionClosed.into());
            };

            match parse_message(&line) {
                Ok(message) => return Ok(message),
                Err(error) => {
                    // The SDK cannot represent this frame. Answer requests with
                    // `-32601` so the client can negotiate down instead of
                    // seeing the connection drop; ignore anything else.
                    if let Some(response) = method_not_found_response(&line) {
                        tracing::debug!(
                            "Replying -32601 to unsupported request: {}",
                            String::from_utf8_lossy(&line)
                        );
                        self.write_line(&response).await?;
                    } else {
                        tracing::debug!("Ignoring unparseable frame: {error}");
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), pmcp::Error> {
        self.closed.store(true, Ordering::Release);
        let mut stdout = self.stdout.lock().await;
        stdout.flush().await.map_err(TransportError::from)?;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    fn transport_type(&self) -> &'static str {
        "stdio"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_discover_request_with_method_not_found() {
        let line = br#"{"jsonrpc":"2.0","id":0,"method":"server/discover"}"#;
        let response = method_not_found_response(line).expect("expected a reply");
        let value: Value = serde_json::from_slice(&response).unwrap();

        assert_eq!(value["id"], 0);
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(value["jsonrpc"], "2.0");
    }

    #[test]
    fn preserves_string_request_ids() {
        let line = br#"{"jsonrpc":"2.0","id":"abc","method":"server/discover"}"#;
        let response = method_not_found_response(line).unwrap();
        let value: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(value["id"], "abc");
    }

    #[test]
    fn never_answers_notifications() {
        let line = br#"{"jsonrpc":"2.0","method":"notifications/unknown"}"#;
        assert!(method_not_found_response(line).is_none());
    }

    #[test]
    fn ignores_non_json_frames() {
        assert!(method_not_found_response(b"not json").is_none());
    }
}
