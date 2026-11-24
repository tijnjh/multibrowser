//! Minimal blocking W3C WebDriver client — just the endpoints the grid needs.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub struct WebDriver {
    agent: ureq::Agent,
    base: String,
    session: Option<String>,
}

impl WebDriver {
    /// `keep_alive: false` opens a fresh connection per request; Ladybird's WebDriver
    /// server mishandles reused connections.
    pub fn new(port: u16, keep_alive: bool) -> Self {
        let agent = ureq::AgentBuilder::new()
            .max_idle_connections(if keep_alive { 8 } else { 0 })
            .timeout_connect(Duration::from_secs(2))
            .timeout(Duration::from_secs(60))
            .build();
        Self { agent, base: format!("http://127.0.0.1:{port}"), session: None }
    }

    /// Wait until the driver's HTTP server answers `/status`.
    pub fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            if self.agent.get(&format!("{}/status", self.base)).call().is_ok() {
                return Ok(());
            }
            if start.elapsed() > timeout {
                bail!("driver did not start listening within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn url(&self, path: &str) -> Result<String> {
        let sid = self.session.as_ref().ok_or_else(|| anyhow!("no session"))?;
        Ok(format!("{}/session/{sid}{path}", self.base))
    }

    fn unwrap(resp: Result<ureq::Response, ureq::Error>) -> Result<Value> {
        let body: Value = match resp {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(_, r)) => r.into_json()?,
            Err(e) => return Err(e.into()),
        };
        let value = body.get("value").cloned().unwrap_or(Value::Null);
        if let Some(err) = value.get("error").and_then(Value::as_str) {
            let msg = value.get("message").and_then(Value::as_str).unwrap_or("");
            bail!("{err}: {msg}");
        }
        Ok(value)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        if std::env::var_os("MB_TRACE_HTTP").is_some() {
            eprintln!("  POST {} {path}", self.base);
        }
        Self::unwrap(
            self.agent
                .post(&self.url(path)?)
                .set("Content-Type", "application/json")
                .send_json(body),
        )
    }

    fn get(&self, path: &str) -> Result<Value> {
        if std::env::var_os("MB_TRACE_HTTP").is_some() && path != "/screenshot" {
            eprintln!("  GET {} {path}", self.base);
        }
        Self::unwrap(self.agent.get(&self.url(path)?).call())
    }

    pub fn new_session(&mut self, always_match: Value) -> Result<()> {
        let v = Self::unwrap(
            self.agent
                .post(&format!("{}/session", self.base))
                .set("Content-Type", "application/json")
                .send_json(json!({ "capabilities": { "alwaysMatch": always_match } })),
        )?;
        let sid = v["sessionId"].as_str().context("no sessionId in response")?;
        self.session = Some(sid.to_string());
        Ok(())
    }

    pub fn delete_session(&mut self) {
        if let Ok(url) = self.url("") {
            let _ = self.agent.delete(&url).timeout(Duration::from_secs(5)).call();
        }
        self.session = None;
    }

    pub fn set_window_rect(&self, rect: Value) -> Result<()> {
        self.post("/window/rect", rect).map(drop)
    }

    pub fn navigate(&self, url: &str) -> Result<()> {
        self.post("/url", json!({ "url": url })).map(drop)
    }
    pub fn back(&self) -> Result<()> {
        self.post("/back", json!({})).map(drop)
    }
    pub fn forward(&self) -> Result<()> {
        self.post("/forward", json!({})).map(drop)
    }
    pub fn refresh(&self) -> Result<()> {
        self.post("/refresh", json!({})).map(drop)
    }

    pub fn current_url(&self) -> Result<String> {
        Ok(self.get("/url")?.as_str().unwrap_or_default().to_string())
    }
    pub fn title(&self) -> Result<String> {
        Ok(self.get("/title")?.as_str().unwrap_or_default().to_string())
    }

    pub fn execute(&self, script: &str, args: Value) -> Result<Value> {
        self.post("/execute/sync", json!({ "script": script, "args": args }))
    }

    /// Viewport PNG, raw bytes.
    pub fn screenshot_png(&self) -> Result<Vec<u8>> {
        let b64 = self.get("/screenshot")?;
        let b64 = b64.as_str().context("screenshot was not a string")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
    }

    pub fn perform_actions(&self, actions: Value) -> Result<()> {
        self.post("/actions", json!({ "actions": actions })).map(drop)
    }
}
