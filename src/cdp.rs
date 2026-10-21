use base64::Engine;
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;

type WsStream = WebSocket<MaybeTlsStream<TcpStream>>;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomMediaItem {
    pub url: String,
    #[serde(rename = "type", default)]
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpResource {
    pub url: String,
    #[serde(rename = "type", default)]
    pub resource_type: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
    #[serde(rename = "contentSize")]
    pub content_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpFrame {
    pub id: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpFrameTree {
    pub frame: CdpFrame,
    #[serde(default)]
    pub resources: Vec<CdpResource>,
    #[serde(rename = "childFrames", default)]
    pub child_frames: Vec<CdpFrameTree>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceTreeResult {
    #[serde(rename = "frameTree")]
    pub frame_tree: CdpFrameTree,
}

#[derive(Clone)]
pub struct CdpClient {
    endpoint: String,
    http_client: ureq::Agent,
    ws_cache: Arc<RwLock<HashMap<String, String>>>,
    ws_pool: Arc<Mutex<HashMap<String, WsStream>>>,
    next_req_id: Arc<AtomicI64>,
}

impl std::fmt::Debug for CdpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpClient")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl CdpClient {
    pub fn new(endpoint: String) -> Self {
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let http_config = ureq::config::Config::builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(5)))
            .build();
        let http_client: ureq::Agent = http_config.into();

        Self {
            endpoint,
            http_client,
            ws_cache: Arc::new(RwLock::new(HashMap::new())),
            ws_pool: Arc::new(Mutex::new(HashMap::new())),
            next_req_id: Arc::new(AtomicI64::new(1)),
        }
    }

    #[allow(dead_code)]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Check if Chrome is reachable
    pub fn is_running(&self) -> bool {
        let url = format!("{}/json/version", self.endpoint);
        self.http_client.get(&url).call().is_ok()
    }

    /// Query the /json/list endpoint to retrieve all targets
    pub fn list_targets(&self) -> Result<Vec<CdpTarget>, CdpError> {
        let url = format!("{}/json/list", self.endpoint);
        let mut resp = match self.http_client.get(&url).call() {
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

        if let Ok(mut cache) = self.ws_cache.write() {
            for target in &targets {
                if let Some(ws_url) = &target.web_socket_debugger_url {
                    cache.insert(target.id.clone(), ws_url.clone());
                }
            }
        }

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
        let ws_url = if let Ok(mut cache) = self.ws_cache.write() {
            cache.remove(target_id)
        } else {
            None
        };
        if let Some(ws_url) = ws_url {
            if let Ok(mut pool) = self.ws_pool.lock() {
                pool.remove(&ws_url);
            }
        }

        let url = format!("{}/json/close/{}", self.endpoint, target_id);
        match self.http_client.get(&url).call() {
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

    /// Prune cached WebSocket URLs and connection pool for targets that are no longer active
    pub fn prune_targets(&self, active_target_ids: &[String]) {
        let active_set: std::collections::HashSet<&str> =
            active_target_ids.iter().map(|s| s.as_str()).collect();
        let mut urls_to_remove = Vec::new();

        if let Ok(mut cache) = self.ws_cache.write() {
            cache.retain(|id, ws_url| {
                let keep = active_set.contains(id.as_str());
                if !keep {
                    urls_to_remove.push(ws_url.clone());
                }
                keep
            });
        }

        if let Ok(mut pool) = self.ws_pool.lock() {
            for ws_url in urls_to_remove {
                pool.remove(&ws_url);
            }
        }
    }

    /// Activate / focus a tab
    pub fn activate_target(&self, target_id: &str) -> Result<(), CdpError> {
        let url = format!("{}/json/activate/{}", self.endpoint, target_id);
        self.http_client
            .get(&url)
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

        let mut resp = self
            .http_client
            .put(&request_url)
            .send_empty()
            .map_err(|e| CdpError::HttpError(format!("Failed to create new tab: {e}")))?;

        let mut body = String::new();
        resp.body_mut()
            .as_reader()
            .read_to_string(&mut body)
            .map_err(|e| CdpError::HttpError(format!("Failed to read response: {e}")))?;

        let target: CdpTarget = serde_json::from_str(&body)
            .map_err(|e| CdpError::HttpError(format!("Failed to parse target JSON: {e}")))?;

        if let Some(ref ws_url) = target.web_socket_debugger_url {
            if let Ok(mut cache) = self.ws_cache.write() {
                cache.insert(target.id.clone(), ws_url.clone());
            }
        }

        Ok(target)
    }

    /// Resolve WebSocket URL for a target ID
    pub fn resolve_ws_url(&self, target_id: &str) -> Result<String, CdpError> {
        if let Ok(cache) = self.ws_cache.read() {
            if let Some(ws_url) = cache.get(target_id) {
                return Ok(ws_url.clone());
            }
        }

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

    fn connect_ws(ws_url: &str) -> Result<WsStream, CdpError> {
        let request =
            tungstenite::client::IntoClientRequest::into_client_request(ws_url).map_err(|e| {
                CdpError::WebSocketError(format!("Invalid WebSocket URL {ws_url}: {e}"))
            })?;
        let uri = request.uri();
        let host = uri
            .host()
            .ok_or_else(|| CdpError::WebSocketError("Missing host in WebSocket URL".to_string()))?;
        let port = uri.port_u16().unwrap_or(80);

        let addrs: Vec<std::net::SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| {
                CdpError::WebSocketError(format!("DNS resolution failed for {host}:{port}: {e}"))
            })?
            .collect();

        let mut stream = None;
        for addr in addrs {
            if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                stream = Some(s);
                break;
            }
        }
        let tcp_stream = stream.ok_or_else(|| {
            CdpError::WebSocketError(format!("Failed to connect to {ws_url} within timeout"))
        })?;

        let _ = tcp_stream.set_nodelay(true);
        let _ = tcp_stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = tcp_stream.set_write_timeout(Some(Duration::from_secs(5)));

        let (socket, _) =
            tungstenite::client(request, MaybeTlsStream::Plain(tcp_stream)).map_err(|e| {
                CdpError::WebSocketError(format!("WebSocket handshake failed to {ws_url}: {e}"))
            })?;

        Ok(socket)
    }

    fn get_or_connect_ws(&self, ws_url: &str) -> Result<WsStream, CdpError> {
        if let Ok(mut pool) = self.ws_pool.lock() {
            if let Some(stream) = pool.remove(ws_url) {
                return Ok(stream);
            }
        }
        Self::connect_ws(ws_url)
    }

    /// Send a command to a target over WebSocket and wait for the response
    pub fn send_ws_command(
        &self,
        ws_url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CdpError> {
        let mut socket = self.get_or_connect_ws(ws_url)?;
        let request_id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        let msg = serde_json::json!({
            "id": request_id,
            "method": method,
            "params": params,
        });
        let text_msg = msg.to_string();

        if let Err(e) = socket.send(tungstenite::Message::Text(text_msg.clone().into())) {
            debug!("Pooled WebSocket send error ({}), reconnecting...", e);
            socket = Self::connect_ws(ws_url)?;
            socket
                .send(tungstenite::Message::Text(text_msg.into()))
                .map_err(|e| {
                    CdpError::WebSocketError(format!("Failed to send WebSocket message: {e}"))
                })?;
        }

        loop {
            let reply = socket.read().map_err(|e| {
                CdpError::TabClosed(format!("WebSocket read failed (tab closed?): {e}"))
            })?;

            if let tungstenite::Message::Text(text) = reply {
                let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                    CdpError::ProtocolError(format!("Invalid JSON in response: {e}"))
                })?;

                if parsed.get("id").and_then(|id| id.as_i64()) == Some(request_id) {
                    if let Ok(mut pool) = self.ws_pool.lock() {
                        pool.insert(ws_url.to_string(), socket);
                    }

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

    /// Retrieve the resource tree of the page (cached and loaded resources)
    pub fn get_resource_tree(&self, ws_url: &str) -> Result<ResourceTreeResult, CdpError> {
        let _ = self.send_ws_command(ws_url, "Page.enable", serde_json::json!({}))?;
        let res = self.send_ws_command(ws_url, "Page.getResourceTree", serde_json::json!({}))?;
        serde_json::from_value(res)
            .map_err(|e| CdpError::ProtocolError(format!("Failed to parse resource tree: {e}")))
    }

    /// Retrieve cached resource content via CDP
    pub fn get_resource_content(
        &self,
        ws_url: &str,
        frame_id: &str,
        url: &str,
    ) -> Result<Vec<u8>, CdpError> {
        let _ = self.send_ws_command(ws_url, "Page.enable", serde_json::json!({}));
        let res = self.send_ws_command(
            ws_url,
            "Page.getResourceContent",
            serde_json::json!({
                "frameId": frame_id,
                "url": url,
            }),
        )?;
        let content = res
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        let base64_encoded = res
            .get("base64Encoded")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        if base64_encoded {
            base64::engine::general_purpose::STANDARD
                .decode(content)
                .map_err(|e| {
                    CdpError::ProtocolError(format!("Failed to decode base64 resource: {e}"))
                })
        } else {
            Ok(content.as_bytes().to_vec())
        }
    }

    /// Query DOM media elements (images, videos, audio)
    pub fn get_dom_media(&self, ws_url: &str) -> Result<Vec<DomMediaItem>, CdpError> {
        let expr = r#"
        (() => {
            const list = [];
            const seen = new Set();
            const add = (url, type) => {
                if (url && typeof url === 'string' && !url.startsWith('about:') && !seen.has(url)) {
                    seen.add(url);
                    list.push({url, type});
                }
            };
            document.querySelectorAll('img, picture source, image').forEach(el => {
                const src = el.currentSrc || el.src || el.getAttribute('src') || el.getAttribute('xlink:href');
                add(src, 'image');
            });
            document.querySelectorAll('video, video source').forEach(el => {
                const src = el.currentSrc || el.src || el.getAttribute('src');
                add(src, 'video');
            });
            document.querySelectorAll('audio, audio source').forEach(el => {
                const src = el.currentSrc || el.src || el.getAttribute('src');
                add(src, 'audio');
            });
            return list;
        })()
        "#;
        let res = self.send_ws_command(
            ws_url,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expr,
                "returnByValue": true,
            }),
        )?;
        let val = res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        serde_json::from_value(val)
            .map_err(|e| CdpError::ProtocolError(format!("Failed to parse DOM media: {e}")))
    }

    /// Fetch asset directly inside the tab context or via direct HTTP streaming
    pub fn fetch_in_tab(&self, ws_url: &str, url: &str) -> Result<Vec<u8>, CdpError> {
        // 1. Data URLs can be decoded directly in Rust without browser or network overhead
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((header, data)) = rest.split_once(',')
        {
            if header.contains(";base64") {
                return base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| {
                        CdpError::ProtocolError(format!("Failed to decode data URL: {e}"))
                    });
            } else {
                return Ok(data.as_bytes().to_vec());
            }
        }

        // 2. For HTTP/HTTPS URLs, stream directly via HTTP client first to avoid heavy V8 base64 marshaling
        if url.starts_with("http://") || url.starts_with("https://") {
            if let Ok(mut resp) = self.http_client.get(url).call() {
                let mut bytes = Vec::new();
                if resp.body_mut().as_reader().read_to_end(&mut bytes).is_ok() && !bytes.is_empty()
                {
                    return Ok(bytes);
                }
            }
        }

        // 3. For blob: URLs or session-authenticated assets where direct HTTP failed,
        // evaluate fetch() inside the tab context
        let json_url = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".to_string());
        let expr = format!(
            r#"
            (async () => {{
                try {{
                    const res = await fetch({});
                    const buf = await res.arrayBuffer();
                    const bytes = new Uint8Array(buf);
                    let bin = '';
                    const chunk = 8192;
                    for (let i = 0; i < bytes.length; i += chunk) {{
                        bin += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
                    }}
                    return btoa(bin);
                }} catch (e) {{
                    return null;
                }}
            }})()
            "#,
            json_url
        );
        let res = self.send_ws_command(
            ws_url,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expr,
                "awaitPromise": true,
                "returnByValue": true,
            }),
        )?;
        if let Some(b64) = res
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
        {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| CdpError::ProtocolError(format!("Failed to decode fetch base64: {e}")))
        } else {
            Err(CdpError::ProtocolError(format!(
                "Unable to fetch asset: {url}"
            )))
        }
    }

    /// Retrieve full asset content lazily in memory
    pub fn get_asset_content(
        &self,
        ws_url: &str,
        frame_id: Option<&str>,
        url: &str,
    ) -> Result<Vec<u8>, CdpError> {
        if let Some(fid) = frame_id
            && let Ok(bytes) = self.get_resource_content(ws_url, fid, url)
            && !bytes.is_empty()
        {
            return Ok(bytes);
        }
        self.fetch_in_tab(ws_url, url)
    }
}
