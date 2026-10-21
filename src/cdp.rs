use base64::Engine;
use log::debug;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("Chrome is not running or unreachable at {0}: {1}")]
    ChromeNotRunning(String, String),

    #[error("Target {0} not found")]
    TargetNotFound(String),

    #[error("Tab was closed: {0}")]
    TabClosed(String),

    #[error("CDP protocol error: {0}")]
    ProtocolError(String),

    #[error("WebSocket error: {0}")]
    WebSocketError(String),

    #[error("HTTP error: {0}")]
    HttpError(String),

    #[allow(dead_code)]
    #[error("Operation timed out")]
    Timeout,
}

impl CdpError {
    pub fn into_errno(&self) -> fuser::Errno {
        match self {
            CdpError::ChromeNotRunning(_, _) => fuser::Errno::ECONNREFUSED,
            CdpError::TargetNotFound(_) | CdpError::TabClosed(_) => fuser::Errno::ENOENT,
            CdpError::Timeout => fuser::Errno::ETIMEDOUT,
            CdpError::ProtocolError(_) | CdpError::WebSocketError(_) | CdpError::HttpError(_) => {
                fuser::Errno::EIO
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "type", default)]
    pub target_type: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CdpClient {
    endpoint: String,
}

impl CdpClient {
    pub fn new(endpoint: String) -> Self {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        Self { endpoint }
    }

    #[allow(dead_code)]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Check if Chrome is reachable
    pub fn is_running(&self) -> bool {
        let url = format!("{}/json/version", self.endpoint);
        ureq::get(&url).call().is_ok()
    }

    /// Query the /json/list endpoint to retrieve all targets
    pub fn list_targets(&self) -> Result<Vec<CdpTarget>, CdpError> {
        let url = format!("{}/json/list", self.endpoint);
        let mut resp = match ureq::get(&url).call() {
            Ok(resp) => resp,
            Err(e) => {
                return Err(CdpError::ChromeNotRunning(
                    self.endpoint.clone(),
                    e.to_string(),
                ));
            }
        };

        let mut body = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .map_err(|e| CdpError::HttpError(format!("Failed to read response: {e}")))?;

        let targets: Vec<CdpTarget> = serde_json::from_str(&body)
            .map_err(|e| CdpError::HttpError(format!("Failed to parse JSON target list: {e}")))?;

        Ok(targets)
    }

    /// List only page tabs (browser tabs)
    pub fn list_tabs(&self) -> Result<Vec<CdpTarget>, CdpError> {
        let targets = self.list_targets()?;
        Ok(targets
            .into_iter()
            .filter(|t| t.target_type == "page")
            .collect())
    }

    /// Close a target by ID
    pub fn close_target(&self, target_id: &str) -> Result<(), CdpError> {
        let url = format!("{}/json/close/{}", self.endpoint, target_id);
        match ureq::get(&url).call() {
            Ok(_) => {
                debug!("Closed target {}", target_id);
                Ok(())
            }
            Err(e) => {
                debug!("Target {} close error: {}", target_id, e);
                // If it's already gone (404), treat as success
                Ok(())
            }
        }
    }

    /// Activate / focus a tab
    pub fn activate_target(&self, target_id: &str) -> Result<(), CdpError> {
        let url = format!("{}/json/activate/{}", self.endpoint, target_id);
        ureq::get(&url)
            .call()
            .map_err(|e| CdpError::HttpError(format!("Failed to activate tab: {e}")))?;
        Ok(())
    }

    /// Create a new tab
    pub fn new_tab(&self, url: &str) -> Result<CdpTarget, CdpError> {
        let request_url = if url.is_empty() {
            format!("{}/json/new", self.endpoint)
        } else {
            format!("{}/json/new?{}", self.endpoint, url)
        };

        let mut resp = ureq::put(&request_url)
            .send_empty()
            .map_err(|e| CdpError::HttpError(format!("Failed to create new tab: {e}")))?;

        let mut body = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .map_err(|e| CdpError::HttpError(format!("Failed to read response: {e}")))?;

        let target: CdpTarget = serde_json::from_str(&body)
            .map_err(|e| CdpError::HttpError(format!("Failed to parse target JSON: {e}")))?;

        Ok(target)
    }

    /// Resolve WebSocket URL for a target ID
    pub fn resolve_ws_url(&self, target_id: &str) -> Result<String, CdpError> {
        let tabs = self.list_targets()?;
        for tab in tabs {
            if tab.id == target_id {
                return tab
                    .web_socket_debugger_url
                    .ok_or_else(|| CdpError::TargetNotFound(target_id.to_string()));
            }
        }
        Err(CdpError::TargetNotFound(target_id.to_string()))
    }

    /// Send a command to a target over WebSocket and wait for the response
    pub fn send_ws_command(
        &self,
        ws_url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CdpError> {
        let (mut socket, _) = tungstenite::connect(ws_url).map_err(|e| {
            CdpError::WebSocketError(format!("WebSocket connect failed to {}: {}", ws_url, e))
        })?;

        // Configure read/write timeouts on the underlying plain TCP stream
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        }

        let request_id = 1;
        let msg = serde_json::json!({
            "id": request_id,
            "method": method,
            "params": params,
        });

        socket
            .send(tungstenite::Message::Text(msg.to_string().into()))
            .map_err(|e| {
                CdpError::WebSocketError(format!("Failed to send WebSocket message: {e}"))
            })?;

        loop {
            let reply = socket.read().map_err(|e| {
                CdpError::TabClosed(format!("WebSocket read failed (tab closed?): {e}"))
            })?;

            if let tungstenite::Message::Text(text) = reply {
                let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                    CdpError::ProtocolError(format!("Invalid JSON in response: {e}"))
                })?;

                if parsed.get("id").and_then(|id| id.as_i64()) == Some(request_id) {
                    if let Some(err) = parsed.get("error") {
                        let err_msg = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error");
                        return Err(CdpError::ProtocolError(err_msg.to_string()));
                    }
                    return Ok(parsed
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null));
                }
            }
        }
    }

    /// Evaluate JavaScript in the tab
    pub fn evaluate(&self, ws_url: &str, expression: &str) -> Result<String, CdpError> {
        let res = self.send_ws_command(
            ws_url,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )?;

        if let Some(exception) = res.get("exceptionDetails") {
            let text = exception
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("Error");
            let desc = exception
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or(text);
            return Ok(format!("Uncaught {desc}"));
        }

        let result_obj = &res["result"];
        if result_obj.is_null() {
            return Ok(String::new());
        }

        if let Some(val) = result_obj.get("value") {
            match val {
                serde_json::Value::String(s) => Ok(s.clone()),
                serde_json::Value::Null => Ok("null".to_string()),
                _ => Ok(val.to_string()),
            }
        } else if let Some(desc) = result_obj.get("description") {
            Ok(desc.as_str().unwrap_or("").to_string())
        } else if let Some(type_str) = result_obj.get("type").and_then(|t| t.as_str()) {
            Ok(type_str.to_string())
        } else {
            Ok(String::new())
        }
    }

    /// Navigate to a URL
    pub fn navigate(&self, ws_url: &str, raw_url: &str) -> Result<(), CdpError> {
        let trimmed = raw_url.trim();
        let target_url = if trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || trimmed.starts_with("about:")
            || trimmed.starts_with("chrome://")
            || trimmed.starts_with("file://")
        {
            trimmed.to_string()
        } else {
            format!("https://{}", trimmed)
        };

        let _ = self.send_ws_command(
            ws_url,
            "Page.navigate",
            serde_json::json!({
                "url": target_url,
            }),
        )?;
        Ok(())
    }

    /// Reload the page
    pub fn reload(&self, ws_url: &str) -> Result<(), CdpError> {
        let _ = self.send_ws_command(
            ws_url,
            "Page.reload",
            serde_json::json!({
                "ignoreCache": true,
            }),
        )?;
        Ok(())
    }

    /// Get current document title
    pub fn get_title(&self, ws_url: &str) -> Result<String, CdpError> {
        self.evaluate(ws_url, "document.title")
    }

    /// Set document title
    pub fn set_title(&self, ws_url: &str, title: &str) -> Result<(), CdpError> {
        let json_title = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string());
        let expr = format!("document.title = {};", json_title);
        let _ = self.evaluate(ws_url, &expr)?;
        Ok(())
    }

    /// Get current URL
    pub fn get_url(&self, ws_url: &str) -> Result<String, CdpError> {
        self.evaluate(ws_url, "window.location.href")
    }

    /// Get page outerHTML
    pub fn get_html(&self, ws_url: &str) -> Result<String, CdpError> {
        self.evaluate(
            ws_url,
            "document.documentElement ? document.documentElement.outerHTML : ''",
        )
    }

    /// Set page innerHTML
    pub fn set_html(&self, ws_url: &str, html: &str) -> Result<(), CdpError> {
        let json_html = serde_json::to_string(html).unwrap_or_else(|_| "\"\"".to_string());
        let expr = format!("document.documentElement.innerHTML = {};", json_html);
        let _ = self.evaluate(ws_url, &expr)?;
        Ok(())
    }

    /// Get innerText (readable plain text)
    pub fn get_text(&self, ws_url: &str) -> Result<String, CdpError> {
        self.evaluate(
            ws_url,
            "document.body ? document.body.innerText : (document.documentElement ? document.documentElement.innerText : '')",
        )
    }

    /// Capture PNG screenshot
    pub fn capture_screenshot(&self, ws_url: &str) -> Result<Vec<u8>, CdpError> {
        let res = self.send_ws_command(
            ws_url,
            "Page.captureScreenshot",
            serde_json::json!({
                "format": "png",
            }),
        )?;

        let b64 = res
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| CdpError::ProtocolError("Missing screenshot data".to_string()))?;

        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| {
                CdpError::ProtocolError(format!("Failed to decode base64 screenshot: {e}"))
            })
    }
}
