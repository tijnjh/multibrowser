//! One background worker per browser engine.
//!
//! Each worker makes sure its engine is downloaded (see `runtime`), starts it, and then
//! loops: apply queued UI commands (navigate, click, scroll, keys, resize) and grab a
//! viewport screenshot that the GPUI grid displays.
//!
//! Blink, Gecko and WebKit run as Playwright's builds via a small Node sidecar;
//! Ladybird runs from its official release via its own WebDriver server.

use crate::runtime;
use crate::webdriver::WebDriver;
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use gpui::RenderImage;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIDECAR_JS: &str = include_str!("sidecar.js");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    Blink,
    Gecko,
    WebKit,
    Ladybird,
}

impl EngineKind {
    pub const ALL: [EngineKind; 4] = [Self::Blink, Self::Gecko, Self::WebKit, Self::Ladybird];

    pub fn name(self) -> &'static str {
        match self {
            Self::Blink => "Blink",
            Self::Gecko => "Gecko",
            Self::WebKit => "WebKit",
            Self::Ladybird => "LibWeb",
        }
    }

    pub fn browser(self) -> &'static str {
        match self {
            Self::Blink => "Chromium",
            Self::Gecko => "Firefox",
            Self::WebKit => "WebKit",
            Self::Ladybird => "Ladybird",
        }
    }

    pub fn accent(self) -> u32 {
        match self {
            Self::Blink => 0x4c8bf5,
            Self::Gecko => 0xff7139,
            Self::WebKit => 0x34aadc,
            Self::Ladybird => 0xb07ee8,
        }
    }

    fn playwright_name(self) -> Option<&'static str> {
        match self {
            Self::Blink => Some("chromium"),
            Self::Gecko => Some("firefox"),
            Self::WebKit => Some("webkit"),
            Self::Ladybird => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Starting(String),
    Loading,
    Ready,
    Error(String),
}

/// State shared between a worker thread and the UI. The UI polls `version`.
#[derive(Default)]
pub struct PaneState {
    pub version: u64,
    pub status: Option<Status>,
    pub engine_version: String,
    pub url: String,
    pub title: String,
    pub frame: Option<Arc<RenderImage>>,
    /// Viewport size in CSS pixels (what click coordinates are relative to).
    pub viewport: (f32, f32),
    pub finished: bool,
    /// For emergency cleanup if the worker is stuck.
    pub child_pid: Option<u32>,
}

/// A key press, named with Playwright/DOM `KeyboardEvent.key` names.
#[derive(Clone, Debug)]
pub enum Key {
    Text(String),
    Named(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    Click,
    Hover,
}

impl PointerAction {
    fn label(self) -> &'static str {
        match self {
            PointerAction::Click => "click",
            PointerAction::Hover => "hover",
        }
    }

    /// How long a follower waits for the leading engine to name the element.
    fn follow_timeout(self) -> Duration {
        match self {
            PointerAction::Click => Duration::from_secs(3),
            // A late hover is worthless, and waiting would stall this engine's frames.
            PointerAction::Hover => Duration::from_millis(300),
        }
    }
}

pub enum Cmd {
    Navigate(String),
    Back,
    Forward,
    Reload,
    /// Viewport size in CSS px.
    Resize(u32, u32),
    /// Click or hover at a point (CSS px in the viewport). With *Sync input* on, the engine
    /// first works out which element is under the point and sends it to each follower.
    Pointer { action: PointerAction, x: f32, y: f32, followers: Vec<Sender<Option<Value>>> },
    /// Click or hover the element another engine reports; if it can't be found, use the
    /// fallback point (CSS px in this viewport).
    PointerFollow { action: PointerAction, x: f32, y: f32, anchor: Receiver<Option<Value>> },
    /// Position and delta in CSS px.
    Scroll { x: f32, y: f32, dx: f32, dy: f32 },
    Keys(Vec<Key>),
    Quit,
}

impl Cmd {
    fn label(&self) -> &'static str {
        match self {
            Cmd::Navigate(_) => "navigate",
            Cmd::Back => "back",
            Cmd::Forward => "forward",
            Cmd::Reload => "reload",
            Cmd::Resize(..) => "resize",
            Cmd::Pointer { action, .. } => action.label(),
            Cmd::PointerFollow { action: PointerAction::Click, .. } => "click (synced)",
            Cmd::PointerFollow { action: PointerAction::Hover, .. } => "hover (synced)",
            Cmd::Scroll { .. } => "scroll",
            Cmd::Keys(_) => "keys",
            Cmd::Quit => "quit",
        }
    }
}

pub struct Engine {
    pub kind: EngineKind,
    pub state: Arc<Mutex<PaneState>>,
    tx: Sender<Cmd>,
}

/// Every engine ever spawned, so signal handlers can shut them down.
static REGISTRY: Mutex<Vec<(Sender<Cmd>, Arc<Mutex<PaneState>>)>> = Mutex::new(Vec::new());

/// Ask every worker to shut its engine down; force-kill stragglers after `timeout`.
pub fn shutdown_all(timeout: Duration) {
    let engines: Vec<_> = REGISTRY.lock().unwrap().clone();
    for (tx, _) in &engines {
        let _ = tx.send(Cmd::Quit);
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && !engines.iter().all(|(_, s)| s.lock().unwrap().finished) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for (_, state) in &engines {
        let s = state.lock().unwrap();
        if let (false, Some(pid)) = (s.finished, s.child_pid) {
            // SIGTERM: the Playwright sidecar and Ladybird's WebDriver both close their browser.
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

impl Engine {
    pub fn spawn(kind: EngineKind) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(PaneState::default()));
        let worker_state = state.clone();
        std::thread::Builder::new()
            .name(format!("engine-{}", kind.name()))
            .spawn(move || {
                let result = run(kind, rx, &worker_state);
                let mut s = worker_state.lock().unwrap();
                if let Err(e) = &result {
                    eprintln!("[{}] error: {e:#}", kind.name());
                    s.status = Some(Status::Error(format!("{e:#}")));
                }
                s.finished = true;
                s.version += 1;
            })
            .expect("spawn engine thread");
        REGISTRY.lock().unwrap().push((tx.clone(), state.clone()));
        Self { kind, state, tx }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

fn trace() -> bool {
    std::env::var_os("MB_TRACE").is_some()
}

fn set_status(kind: EngineKind, state: &Mutex<PaneState>, status: Status) {
    let mut s = state.lock().unwrap();
    if s.status.as_ref() != Some(&status) {
        if trace() || !matches!(status, Status::Starting(ref m) if m.contains('%')) {
            eprintln!("[{}] {status:?}", kind.name());
        }
        s.status = Some(status);
        s.version += 1;
    }
}

// ---------------------------------------------------------------------------------------
// Backends

struct Meta {
    url: String,
    title: String,
    viewport: Option<(f32, f32)>,
}

trait Backend {
    fn navigate(&mut self, url: &str) -> Result<()>;
    fn back(&mut self) -> Result<()>;
    fn forward(&mut self) -> Result<()>;
    fn reload(&mut self) -> Result<()>;
    fn resize(&mut self, w: u32, h: u32) -> Result<()>;
    fn click(&mut self, x: f32, y: f32) -> Result<()>;
    fn hover(&mut self, x: f32, y: f32) -> Result<()>;
    fn scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> Result<()>;
    fn keys(&mut self, keys: &[Key]) -> Result<()>;
    /// Calls the JS function `func` (source text) in the page with `args` (a JSON array).
    fn call_js(&mut self, func: &str, args: Value) -> Result<Value>;
    fn screenshot_png(&mut self) -> Result<Vec<u8>>;
    fn meta(&mut self) -> Result<Meta>;
    fn shutdown(&mut self);
}

/// Blink / Gecko / WebKit through a Playwright sidecar running on our own Node.js.
struct PlaywrightBackend {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl PlaywrightBackend {
    fn start(kind: EngineKind, state: &Mutex<PaneState>) -> Result<(Self, String)> {
        let engine = kind.playwright_name().unwrap();
        let report = |m: String| set_status(kind, state, Status::Starting(m));
        let rt = runtime::ensure_playwright(&report)?;
        runtime::ensure_playwright_browser(&rt, engine, &report)?;

        let script = runtime::data_dir().join(format!("sidecar-{}.js", runtime::PLAYWRIGHT_VERSION));
        if std::fs::read_to_string(&script).ok().as_deref() != Some(SIDECAR_JS) {
            std::fs::write(&script, SIDECAR_JS)?;
        }
        report(format!("Launching {}…", kind.browser()));
        let mut child = Command::new(&rt.node)
            .arg(&script)
            .arg(&rt.package)
            .arg(engine)
            .env("PLAYWRIGHT_BROWSERS_PATH", &rt.browsers)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if trace() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .context("starting Playwright sidecar")?;
        state.lock().unwrap().child_pid = Some(child.id());
        let mut me = Self {
            stdin: BufWriter::new(child.stdin.take().unwrap()),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 0,
        };
        let version = me.call("launch", json!({}))?;
        Ok((me, version.as_str().unwrap_or_default().to_string()))
    }

    fn call(&mut self, cmd: &str, mut args: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        args["id"] = json!(id);
        args["cmd"] = json!(cmd);
        serde_json::to_writer(&mut self.stdin, &args)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("Playwright sidecar exited");
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg["id"].as_u64() != Some(id) {
                continue;
            }
            return if msg["ok"].as_bool() == Some(true) {
                Ok(msg["value"].clone())
            } else {
                Err(anyhow!("{}", msg["error"].as_str().unwrap_or("unknown error")))
            };
        }
    }
}

impl Backend for PlaywrightBackend {
    fn navigate(&mut self, url: &str) -> Result<()> {
        self.call("navigate", json!({ "url": url })).map(drop)
    }
    fn back(&mut self) -> Result<()> {
        self.call("back", json!({})).map(drop)
    }
    fn forward(&mut self) -> Result<()> {
        self.call("forward", json!({})).map(drop)
    }
    fn reload(&mut self) -> Result<()> {
        self.call("reload", json!({})).map(drop)
    }
    fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        self.call("resize", json!({ "width": w, "height": h })).map(drop)
    }
    fn click(&mut self, x: f32, y: f32) -> Result<()> {
        self.call("click", json!({ "x": x, "y": y })).map(drop)
    }
    fn hover(&mut self, x: f32, y: f32) -> Result<()> {
        self.call("hover", json!({ "x": x, "y": y })).map(drop)
    }
    fn scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> Result<()> {
        self.call("scroll", json!({ "x": x, "y": y, "dx": dx, "dy": dy })).map(drop)
    }
    fn keys(&mut self, keys: &[Key]) -> Result<()> {
        let keys: Vec<Value> = keys
            .iter()
            .map(|k| match k {
                Key::Text(t) => json!({ "text": t }),
                Key::Named(n) => json!({ "key": n }),
            })
            .collect();
        self.call("keys", json!({ "keys": keys })).map(drop)
    }
    fn call_js(&mut self, func: &str, args: Value) -> Result<Value> {
        // An expression string, so the page's CSP can't block it the way it could `new Function`.
        self.call("evaluate", json!({ "expr": format!("({func})(...{args})") }))
    }
    fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        let b64 = self.call("screenshot", json!({}))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64.as_str().unwrap_or_default())?)
    }
    fn meta(&mut self) -> Result<Meta> {
        let v = self.call("meta", json!({}))?;
        Ok(Meta {
            url: v["url"].as_str().unwrap_or_default().into(),
            title: v["title"].as_str().unwrap_or_default().into(),
            viewport: Some((v["width"].as_f64().unwrap_or(0.0) as f32, v["height"].as_f64().unwrap_or(0.0) as f32)),
        })
    }
    fn shutdown(&mut self) {
        let _ = self.call("quit", json!({}));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ladybird through its bundled WebDriver server.
struct LadybirdBackend {
    wd: WebDriver,
    driver: Child,
    wheel_supported: bool,
}

impl LadybirdBackend {
    fn start(kind: EngineKind, state: &Mutex<PaneState>) -> Result<(Self, String)> {
        let report = |m: String| set_status(kind, state, Status::Starting(m));
        let webdriver = runtime::ensure_ladybird(&report)?;
        report("Launching Ladybird…".into());
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let driver = Command::new(&webdriver)
            .args(["--port", &port.to_string(), "--headless"])
            .envs(runtime::ladybird_env(&webdriver))
            .stdout(Stdio::null())
            .stderr(if trace() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .with_context(|| format!("launching {}", webdriver.display()))?;
        state.lock().unwrap().child_pid = Some(driver.id());
        // Ladybird's WebDriver server mishandles reused keep-alive connections.
        let mut me = Self { wd: WebDriver::new(port, false), driver, wheel_supported: true };
        me.wd.wait_ready(Duration::from_secs(15))?;
        me.wd.new_session(json!({ "pageLoadStrategy": "eager" }))?;
        let version = webdriver
            .ancestors()
            .map(|p| p.join("VERSION"))
            .find(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        Ok((me, version.trim().to_string()))
    }

    fn inner_size(&self) -> Option<(f32, f32)> {
        let v = self.wd.execute("return [window.innerWidth, window.innerHeight];", json!([])).ok()?;
        Some((v.get(0)?.as_f64()? as f32, v.get(1)?.as_f64()? as f32))
    }
}

fn webdriver_key(k: &Key) -> Vec<String> {
    match k {
        Key::Text(t) => t.chars().map(String::from).collect(),
        Key::Named(n) => {
            let code = match *n {
                "Enter" => "\u{E007}",
                "Backspace" => "\u{E003}",
                "Tab" => "\u{E004}",
                "Escape" => "\u{E00C}",
                "Delete" => "\u{E017}",
                "ArrowLeft" => "\u{E012}",
                "ArrowUp" => "\u{E013}",
                "ArrowRight" => "\u{E014}",
                "ArrowDown" => "\u{E015}",
                "PageUp" => "\u{E00E}",
                "PageDown" => "\u{E00F}",
                "Home" => "\u{E011}",
                "End" => "\u{E010}",
                _ => return vec![],
            };
            vec![code.to_string()]
        }
    }
}

impl Backend for LadybirdBackend {
    fn navigate(&mut self, url: &str) -> Result<()> {
        self.wd.navigate(url)
    }
    fn back(&mut self) -> Result<()> {
        self.wd.back()
    }
    fn forward(&mut self) -> Result<()> {
        self.wd.forward()
    }
    fn reload(&mut self) -> Result<()> {
        self.wd.refresh()
    }
    fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        // Size the window so the *viewport* ends up w×h CSS px.
        let (mut ow, mut oh) = (w as i64, h as i64);
        for _ in 0..2 {
            self.wd.set_window_rect(json!({ "width": ow, "height": oh }))?;
            let Some((iw, ih)) = self.inner_size() else { break };
            let (dw, dh) = (w as i64 - iw as i64, h as i64 - ih as i64);
            if dw.abs() <= 1 && dh.abs() <= 1 {
                break;
            }
            ow += dw;
            oh += dh;
        }
        Ok(())
    }
    fn click(&mut self, x: f32, y: f32) -> Result<()> {
        self.wd.perform_actions(json!([{
            "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" },
            "actions": [
                { "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport", "duration": 0 },
                { "type": "pointerDown", "button": 0 },
                { "type": "pointerUp", "button": 0 },
            ]
        }]))
    }
    fn hover(&mut self, x: f32, y: f32) -> Result<()> {
        self.wd.perform_actions(json!([{
            "type": "pointer", "id": "mouse", "parameters": { "pointerType": "mouse" },
            "actions": [
                { "type": "pointerMove", "x": x.round() as i64, "y": y.round() as i64, "origin": "viewport", "duration": 0 },
            ]
        }]))
    }
    fn scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> Result<()> {
        if self.wheel_supported {
            let r = self.wd.perform_actions(json!([{
                "type": "wheel", "id": "wheel",
                "actions": [{
                    "type": "scroll", "origin": "viewport", "duration": 0,
                    "x": x.round() as i64, "y": y.round() as i64,
                    "deltaX": dx.round() as i64, "deltaY": dy.round() as i64,
                }]
            }]));
            if r.is_ok() {
                return Ok(());
            }
            self.wheel_supported = false;
        }
        self.wd.execute("window.scrollBy(arguments[0], arguments[1]);", json!([dx, dy])).map(drop)
    }
    fn keys(&mut self, keys: &[Key]) -> Result<()> {
        let mut actions = Vec::new();
        for k in keys.iter().flat_map(webdriver_key) {
            actions.push(json!({ "type": "keyDown", "value": k }));
            actions.push(json!({ "type": "keyUp", "value": k }));
        }
        self.wd.perform_actions(json!([{ "type": "key", "id": "keyboard", "actions": actions }]))
    }
    fn call_js(&mut self, func: &str, args: Value) -> Result<Value> {
        self.wd.execute(&format!("return ({func}).apply(null, arguments);"), args)
    }
    fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        self.wd.screenshot_png()
    }
    fn meta(&mut self) -> Result<Meta> {
        Ok(Meta {
            url: self.wd.current_url().unwrap_or_default(),
            title: self.wd.title().unwrap_or_default(),
            viewport: self.inner_size(),
        })
    }
    fn shutdown(&mut self) {
        self.wd.delete_session();
        let _ = self.driver.kill();
        let _ = self.driver.wait();
    }
}

// ---------------------------------------------------------------------------------------
// Synced clicks
//
// Engines lay pages out slightly differently, so the same point can be over different
// elements. The engine that was clicked describes the element under the pointer as a
// selector path plus where inside its box the click landed; the others click that same
// spot on their copy of the element.

/// `(x, y)` → `{ selector, fx, fy }` or null.
const ANCHOR_JS: &str = r#"function (x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el) return null;
  const path = [];
  for (let e = el; e; e = e.parentElement) {
    if (e === document.documentElement) { path.unshift(':root'); break; }
    if (e.id && document.querySelectorAll('#' + CSS.escape(e.id)).length === 1) {
      path.unshift('#' + CSS.escape(e.id));
      break;
    }
    let i = 1;
    for (let s = e.previousElementSibling; s; s = s.previousElementSibling) {
      if (s.localName === e.localName) i++;
    }
    path.unshift(CSS.escape(e.localName) + ':nth-of-type(' + i + ')');
  }
  const r = el.getBoundingClientRect();
  return {
    selector: path.join(' > '),
    fx: r.width > 0 ? (x - r.left) / r.width : 0.5,
    fy: r.height > 0 ? (y - r.top) / r.height : 0.5,
  };
}"#;

/// `({ selector, fx, fy }, scroll)` → `{ x, y }` in this viewport, or null. With `scroll`
/// (clicks), an off-screen element is scrolled into view first; hovers never scroll the page.
const RESOLVE_JS: &str = r#"function (a, scroll) {
  let el;
  try { el = document.querySelector(a.selector); } catch (e) { return null; }
  if (!el) return null;
  let r = el.getBoundingClientRect();
  if (scroll && (r.bottom <= 0 || r.top >= innerHeight || r.right <= 0 || r.left >= innerWidth)) {
    el.scrollIntoView({ block: 'center', inline: 'nearest' });
    r = el.getBoundingClientRect();
  }
  const clamp = (v) => Math.min(Math.max(v, 0), 1);
  const x = r.left + clamp(a.fx) * r.width, y = r.top + clamp(a.fy) * r.height;
  if (x < 0 || y < 0 || x > innerWidth || y > innerHeight) return null;
  return { x, y };
}"#;

// ---------------------------------------------------------------------------------------
// Worker loop

fn run(kind: EngineKind, rx: Receiver<Cmd>, state: &Arc<Mutex<PaneState>>) -> Result<()> {
    let (backend, version): (Box<dyn Backend>, String) = match kind {
        EngineKind::Ladybird => {
            let (b, v) = LadybirdBackend::start(kind, state)?;
            (Box::new(b), v)
        }
        _ => {
            let (b, v) = PlaywrightBackend::start(kind, state)?;
            (Box::new(b), v)
        }
    };
    {
        let mut s = state.lock().unwrap();
        s.engine_version = version;
        s.version += 1;
    }
    set_status(kind, state, Status::Ready);
    let mut w = Worker { kind, state, backend, last_png_hash: 0 };
    let r = w.event_loop(rx);
    w.backend.shutdown();
    r
}

struct Worker<'a> {
    kind: EngineKind,
    state: &'a Mutex<PaneState>,
    backend: Box<dyn Backend>,
    last_png_hash: u64,
}

impl Worker<'_> {
    fn event_loop(&mut self, rx: Receiver<Cmd>) -> Result<()> {
        let mut idle_frames = 0u32;
        let mut last_meta = Instant::now() - Duration::from_secs(10);
        loop {
            // Adaptive frame pacing: fast while things change, slower when static.
            let wait = Duration::from_millis(match idle_frames {
                0..=20 => 16,
                21..=60 => 100,
                _ => 300,
            });
            let mut cmds = Vec::new();
            match rx.recv_timeout(wait) {
                Ok(c) => cmds.push(c),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            cmds.extend(rx.try_iter());
            if !cmds.is_empty() {
                idle_frames = 0;
            }
            if self.apply(cmds) {
                return Ok(());
            }

            if last_meta.elapsed() > Duration::from_millis(500) {
                last_meta = Instant::now();
                self.refresh_meta();
            }

            match self.backend.screenshot_png() {
                Ok(png) => {
                    let mut h = DefaultHasher::new();
                    png.hash(&mut h);
                    let hash = h.finish();
                    if hash == self.last_png_hash {
                        idle_frames += 1;
                        continue;
                    }
                    idle_frames = 0;
                    self.last_png_hash = hash;
                    if let Some(dir) = std::env::var_os("MB_DUMP") {
                        let _ = std::fs::write(Path::new(&dir).join(format!("{}.png", self.kind.name())), &png);
                    }
                    if let Ok(img) = decode(&png) {
                        let mut s = self.state.lock().unwrap();
                        s.frame = Some(img);
                        s.version += 1;
                    }
                }
                Err(e) => {
                    if trace() {
                        eprintln!("[{}] screenshot: {e:#}", self.kind.name());
                    }
                    idle_frames += 1;
                }
            }
        }
    }

    /// Returns true if the worker should quit.
    fn apply(&mut self, cmds: Vec<Cmd>) -> bool {
        // Coalesce resizes (keep last) and scrolls (sum) so a burst costs one round-trip.
        let mut resize = None;
        let mut scroll: Option<(f32, f32, f32, f32)> = None;
        let mut rest = Vec::new();
        for c in cmds {
            match c {
                Cmd::Quit => return true,
                Cmd::Resize(w, h) => resize = Some((w, h)),
                Cmd::Scroll { x, y, dx, dy } => {
                    let s = scroll.get_or_insert((x, y, 0.0, 0.0));
                    *s = (x, y, s.2 + dx, s.3 + dy);
                }
                other => {
                    // Only the latest of a run of hovers matters; dropping one drops its
                    // channels, so its followers fall back to coordinates without waiting.
                    let hover = |c: &Cmd| {
                        matches!(
                            c,
                            Cmd::Pointer { action: PointerAction::Hover, .. }
                                | Cmd::PointerFollow { action: PointerAction::Hover, .. }
                        )
                    };
                    if hover(&other) && rest.last().is_some_and(hover) {
                        rest.pop();
                    }
                    rest.push(other)
                }
            }
        }
        if let Some((w, h)) = resize {
            if trace() {
                eprintln!("[{}] resize {w}x{h}", self.kind.name());
            }
            if let Err(e) = self.backend.resize(w, h) {
                eprintln!("[{}] resize failed: {e:#}", self.kind.name());
            }
            self.refresh_meta();
        }
        for c in rest {
            if trace() {
                eprintln!("[{}] cmd {}", self.kind.name(), c.label());
            }
            let is_nav = matches!(c, Cmd::Navigate(_) | Cmd::Back | Cmd::Forward | Cmd::Reload);
            if is_nav {
                set_status(self.kind, self.state, Status::Loading);
            }
            let r = match c {
                Cmd::Navigate(url) => self.backend.navigate(&url),
                Cmd::Back => self.backend.back(),
                Cmd::Forward => self.backend.forward(),
                Cmd::Reload => self.backend.reload(),
                Cmd::Pointer { action, x, y, followers } => {
                    if !followers.is_empty() {
                        let anchor = self.backend.call_js(ANCHOR_JS, json!([x, y])).ok().filter(|a| !a.is_null());
                        for f in followers {
                            let _ = f.send(anchor.clone());
                        }
                    }
                    self.pointer(action, x, y)
                }
                Cmd::PointerFollow { action, x, y, anchor } => {
                    // The leading engine handles its pointer command in order with its other
                    // commands, so this normally arrives within one round-trip. Don't hang if
                    // it's stuck. A disconnected channel means it skipped a stale hover.
                    let found = anchor
                        .recv_timeout(action.follow_timeout())
                        .ok()
                        .flatten()
                        .and_then(|a| self.backend.call_js(RESOLVE_JS, json!([a, action == PointerAction::Click])).ok())
                        .and_then(|p| Some((p["x"].as_f64()? as f32, p["y"].as_f64()? as f32)));
                    if trace() && found.is_none() {
                        eprintln!("[{}] synced {}: element not found, using coordinates", self.kind.name(), action.label());
                    }
                    let (x, y) = found.unwrap_or((x, y));
                    self.pointer(action, x, y)
                }
                Cmd::Keys(keys) => self.backend.keys(&keys),
                Cmd::Resize(..) | Cmd::Scroll { .. } | Cmd::Quit => unreachable!(),
            };
            if is_nav {
                let status = match &r {
                    Ok(()) => Status::Ready,
                    Err(e) => Status::Error(format!("{e:#}")),
                };
                set_status(self.kind, self.state, status);
                self.refresh_meta();
            } else if let Err(e) = r {
                eprintln!("[{}] command failed: {e:#}", self.kind.name());
            }
        }
        if let Some((x, y, dx, dy)) = scroll
            && let Err(e) = self.backend.scroll(x, y, dx, dy)
        {
            eprintln!("[{}] scroll failed: {e:#}", self.kind.name());
        }
        false
    }

    fn pointer(&mut self, action: PointerAction, x: f32, y: f32) -> Result<()> {
        match action {
            PointerAction::Click => self.backend.click(x, y),
            PointerAction::Hover => self.backend.hover(x, y),
        }
    }

    fn refresh_meta(&mut self) {
        let Ok(meta) = self.backend.meta() else { return };
        let mut s = self.state.lock().unwrap();
        let mut changed = false;
        if s.url != meta.url {
            s.url = meta.url;
            changed = true;
        }
        if s.title != meta.title {
            s.title = meta.title;
            changed = true;
        }
        if let Some(vp) = meta.viewport
            && s.viewport != vp
        {
            s.viewport = vp;
            changed = true;
        }
        if changed {
            s.version += 1;
        }
    }
}

fn decode(png: &[u8]) -> Result<Arc<RenderImage>> {
    let mut rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
    // GPUI expects BGRA.
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}
