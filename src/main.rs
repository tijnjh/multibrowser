mod engine;
mod runtime;
mod webdriver;

use engine::{Cmd, Engine, EngineKind, Key, PointerAction, Status};
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, Pixels, RenderImage, ScrollWheelEvent, SharedString, Window,
    WindowBounds, WindowOptions, canvas, div, img, prelude::*, px, rgb, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://en.wikipedia.org/wiki/Comparison_of_browser_engines";

/// What the UI currently shows for one engine.
struct Pane {
    engine: Engine,
    focus: FocusHandle,
    seen_version: u64,
    status: Option<Status>,
    url: String,
    title: String,
    engine_version: String,
    viewport: (f32, f32),
    frame: Option<Arc<RenderImage>>,
    /// Bounds of the screenshot area, captured during prepaint.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    sent_size: (u32, u32),
    pending_size: Option<((u32, u32), Instant)>,
}

struct MultiBrowser {
    panes: Vec<Pane>,
    url_text: String,
    url_select_all: bool,
    url_focus: FocusHandle,
    zoom: f32,
    /// Mirror clicks, keys and scrolls in one pane to every pane.
    sync_input: bool,
}

impl MultiBrowser {
    fn new(initial_url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panes = EngineKind::ALL
            .into_iter()
            .map(|kind| {
                let engine = Engine::spawn(kind);
                engine.send(Cmd::Navigate(initial_url.clone()));
                Pane {
                    engine,
                    focus: cx.focus_handle(),
                    seen_version: 0,
                    status: None,
                    url: String::new(),
                    title: String::new(),
                    engine_version: String::new(),
                    viewport: (0.0, 0.0),
                    frame: None,
                    bounds: Rc::new(Cell::new(None)),
                    sent_size: (0, 0),
                    pending_size: None,
                }
            })
            .collect();

        // Poll the engine workers at ~60 Hz.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                if this.update_in(cx, |this, window, cx| this.poll(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let url_focus = cx.focus_handle();
        window.focus(&url_focus);
        Self {
            panes,
            url_text: initial_url,
            url_select_all: true,
            url_focus,
            zoom: 1.0,
            sync_input: true,
        }
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        // Every engine lays out as if its browser window filled this whole window;
        // the pane then shows that frame scaled down.
        let full_width = f32::from(window.viewport_size().width) * self.zoom;
        for pane in &mut self.panes {
            {
                let s = pane.engine.state.lock().unwrap();
                if s.version != pane.seen_version {
                    pane.seen_version = s.version;
                    pane.status = s.status.clone();
                    pane.url = s.url.clone();
                    pane.title = s.title.clone();
                    pane.viewport = s.viewport;
                    pane.engine_version = s.engine_version.clone();
                    let new_frame = s.frame.clone();
                    drop(s);
                    let same = match (&pane.frame, &new_frame) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    if !same && let Some(old) = std::mem::replace(&mut pane.frame, new_frame) {
                        // Free the GPU texture of the frame we no longer show.
                        let _ = window.drop_image(old);
                    }
                    changed = true;
                }
            }

            // Viewport: full window width, pane aspect ratio (debounced).
            if let Some(b) = pane.bounds.get() {
                let (bw, bh) = (f32::from(b.size.width).max(1.0), f32::from(b.size.height));
                let want = (
                    full_width.round().max(320.0) as u32,
                    (bh * full_width / bw).round().max(200.0) as u32,
                );
                if want != pane.sent_size {
                    match pane.pending_size {
                        Some((sz, since)) if sz == want => {
                            if since.elapsed() > Duration::from_millis(200) {
                                pane.engine.send(Cmd::Resize(want.0, want.1));
                                pane.sent_size = want;
                                pane.pending_size = None;
                            }
                        }
                        _ => pane.pending_size = Some((want, Instant::now())),
                    }
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn broadcast(&self, make: impl Fn() -> Cmd) {
        for p in &self.panes {
            p.engine.send(make());
        }
    }

    /// Click or hover pane `i` at a page point; with *Sync input* on, the other engines do
    /// the same on the same element (or, if they can't find it, at the same relative point).
    fn pointer(&self, i: usize, action: PointerAction, x: f32, y: f32) {
        if !self.sync_input {
            self.panes[i].engine.send(Cmd::Pointer { action, x, y, followers: Vec::new() });
            return;
        }
        let (sw, sh) = self.panes[i].viewport;
        let rel = (x / sw.max(1.0), y / sh.max(1.0));
        let mut followers = Vec::new();
        for (j, p) in self.panes.iter().enumerate() {
            if j == i {
                continue;
            }
            let (tx, rx) = mpsc::channel();
            followers.push(tx);
            let (vw, vh) = p.viewport;
            p.engine.send(Cmd::PointerFollow { action, x: rel.0 * vw, y: rel.1 * vh, anchor: rx });
        }
        self.panes[i].engine.send(Cmd::Pointer { action, x, y, followers });
    }

    fn go(&mut self, cx: &mut Context<Self>) {
        let url = normalize_url(&self.url_text);
        self.url_text = url.clone();
        self.url_select_all = true;
        self.broadcast(|| Cmd::Navigate(url.clone()));
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.zoom = zoom.clamp(0.5, 3.0);
        cx.notify();
    }

    /// How much pane `i` shrinks its page (pane px per CSS px), e.g. 0.5.
    fn page_scale(&self, i: usize) -> Option<f32> {
        let pane = &self.panes[i];
        let b = pane.bounds.get()?;
        (pane.viewport.0 > 0.0).then(|| f32::from(b.size.width) / pane.viewport.0)
    }

    /// Map a window-space position to CSS px inside pane `i`'s viewport.
    fn to_page(&self, i: usize, pos: gpui::Point<Pixels>) -> Option<(f32, f32)> {
        let pane = &self.panes[i];
        let b = pane.bounds.get()?;
        let frame = pane.frame.as_ref()?;
        let fs = frame.size(0);
        let (iw, ih) = (fs.width.0 as f32, fs.height.0 as f32);
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        // Same math as ObjectFit::Contain.
        let scale = (bw / iw).min(bh / ih);
        let (fw, fh) = (iw * scale, ih * scale);
        let ox = f32::from(b.origin.x) + (bw - fw) / 2.0;
        let oy = f32::from(b.origin.y) + (bh - fh) / 2.0;
        let (rx, ry) = ((f32::from(pos.x) - ox) / fw, (f32::from(pos.y) - oy) / fh);
        if !(0.0..=1.0).contains(&rx) || !(0.0..=1.0).contains(&ry) {
            return None;
        }
        let (vw, vh) = if pane.viewport.0 > 0.0 { pane.viewport } else { (iw, ih) };
        Some((rx * vw, ry * vh))
    }

    fn on_url_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        if m.platform {
            match ks.key.as_str() {
                "a" => self.url_select_all = true,
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        let text = text.trim().replace('\n', "");
                        if self.url_select_all {
                            self.url_text.clear();
                        }
                        self.url_text.push_str(&text);
                        self.url_select_all = false;
                    }
                }
                "c" => cx.write_to_clipboard(ClipboardItem::new_string(self.url_text.clone())),
                "backspace" => {
                    self.url_text.clear();
                    self.url_select_all = false;
                }
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "enter" => self.go(cx),
            "escape" => {
                if let Some(p) = self.panes.iter().find(|p| !p.url.is_empty()) {
                    self.url_text = p.url.clone();
                }
                self.url_select_all = true;
            }
            "backspace" => {
                if self.url_select_all {
                    self.url_text.clear();
                } else {
                    self.url_text.pop();
                }
                self.url_select_all = false;
            }
            _ => match &ks.key_char {
                Some(ch) if !m.control && !ch.chars().any(char::is_control) => {
                    if self.url_select_all {
                        self.url_text.clear();
                    }
                    self.url_text.push_str(ch);
                    self.url_select_all = false;
                }
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_pane_key(&mut self, i: usize, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.platform || ks.modifiers.control {
            return; // leave shortcuts to the app
        }
        let named = match ks.key.as_str() {
            "enter" => Some("Enter"),
            "backspace" => Some("Backspace"),
            "tab" => Some("Tab"),
            "escape" => Some("Escape"),
            "delete" => Some("Delete"),
            "left" => Some("ArrowLeft"),
            "up" => Some("ArrowUp"),
            "right" => Some("ArrowRight"),
            "down" => Some("ArrowDown"),
            "pageup" => Some("PageUp"),
            "pagedown" => Some("PageDown"),
            "home" => Some("Home"),
            "end" => Some("End"),
            _ => None,
        };
        let key = match (named, &ks.key_char) {
            (Some(n), _) => Key::Named(n),
            (None, Some(ch)) => Key::Text(ch.clone()),
            (None, None) if ks.key == "space" => Key::Text(" ".into()),
            (None, None) => return,
        };
        // Each engine types into whatever it has focused, which synced clicks keep in step.
        if self.sync_input {
            self.broadcast(|| Cmd::Keys(vec![key.clone()]));
        } else {
            self.panes[i].engine.send(Cmd::Keys(vec![key]));
        }
        cx.stop_propagation();
    }

    fn on_root_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if !ks.modifiers.platform {
            return;
        }
        match ks.key.as_str() {
            "l" => {
                window.focus(&self.url_focus);
                self.url_select_all = true;
            }
            "r" => self.broadcast(|| Cmd::Reload),
            "[" => self.broadcast(|| Cmd::Back),
            "]" => self.broadcast(|| Cmd::Forward),
            "=" | "+" => self.set_zoom(self.zoom + 0.25, cx),
            "-" => self.set_zoom(self.zoom - 0.25, cx),
            "0" => self.set_zoom(1.0, cx),
            "q" => cx.quit(),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.url_focus.is_focused(window);
        let url_display: SharedString = if self.url_text.is_empty() && !focused {
            "Enter a URL or search…".into()
        } else {
            self.url_text.clone().into()
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(0x181825))
            .border_b_1()
            .border_color(rgb(0x313244))
            .child(button("back", "←", cx.listener(|this, _, _, _| this.broadcast(|| Cmd::Back))))
            .child(button("fwd", "→", cx.listener(|this, _, _, _| this.broadcast(|| Cmd::Forward))))
            .child(button("reload", "⟳", cx.listener(|this, _, _, _| this.broadcast(|| Cmd::Reload))))
            .child(
                div()
                    .id("url")
                    .track_focus(&self.url_focus)
                    .on_key_down(cx.listener(Self::on_url_key))
                    .on_click(cx.listener(|this, _, window, cx| {
                        window.focus(&this.url_focus);
                        cx.notify();
                    }))
                    .flex_1()
                    .flex()
                    .items_center()
                    .h_8()
                    .px_3()
                    .rounded_md()
                    .bg(rgb(0x11111b))
                    .border_1()
                    .border_color(rgb(if focused { 0x89b4fa } else { 0x313244 }))
                    .text_sm()
                    .overflow_hidden()
                    .child(
                        div()
                            .whitespace_nowrap()
                            .when(focused && self.url_select_all && !self.url_text.is_empty(), |d| {
                                d.bg(rgb(0x45475a))
                            })
                            .text_color(rgb(if self.url_text.is_empty() { 0x6c7086 } else { 0xcdd6f4 }))
                            .child(url_display),
                    )
                    .when(focused && !self.url_select_all, |d| {
                        d.child(div().w(px(1.5)).h_4().bg(rgb(0x89b4fa)))
                    }),
            )
            .child(button("go", "Go", cx.listener(|this, _, _, cx| this.go(cx))))
            .child(div().w_2())
            .child(button("zoom-out", "−", cx.listener(|this, _, _, cx| this.set_zoom(this.zoom - 0.25, cx))))
            .child(
                div()
                    .w_12()
                    .text_center()
                    .text_xs()
                    .text_color(rgb(0xa6adc8))
                    .child(match self.page_scale(0) {
                        Some(s) => format!("{:.0}%", s * 100.0),
                        None => "–".into(),
                    }),
            )
            .child(button("zoom-in", "+", cx.listener(|this, _, _, cx| this.set_zoom(this.zoom + 0.25, cx))))
            .child(button(
                "sync",
                if self.sync_input { "Sync input: on" } else { "Sync input: off" },
                cx.listener(|this, _, _, cx| {
                    this.sync_input = !this.sync_input;
                    cx.notify();
                }),
            ))
    }

    fn render_pane(&self, i: usize, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.panes[i];
        let kind = pane.engine.kind;
        let focused = pane.focus.is_focused(window);
        let (status_text, status_color) = match &pane.status {
            None => ("…".to_string(), 0x6c7086),
            Some(Status::Starting(s)) => (s.clone(), 0xf9e2af),
            Some(Status::Loading) => ("Loading…".into(), 0xf9e2af),
            Some(Status::Ready) => ("Ready".into(), 0xa6e3a1),
            Some(Status::Error(_)) => ("Error".into(), 0xf38ba8),
        };
        let bounds_cell = pane.bounds.clone();

        let body = div()
            .id(("pane-body", i))
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0xffffff))
            .track_focus(&pane.focus)
            .on_key_down(cx.listener(move |this, ev, window, cx| this.on_pane_key(i, ev, window, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.panes[i].focus);
                    if let Some((x, y)) = this.to_page(i, ev.position) {
                        this.pointer(i, PointerAction::Click, x, y);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, _| {
                if ev.pressed_button.is_some() {
                    return; // drags aren't forwarded
                }
                if let Some((x, y)) = this.to_page(i, ev.position) {
                    this.pointer(i, PointerAction::Hover, x, y);
                }
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, window, _| {
                let d = ev.delta.pixel_delta(window.line_height());
                // GPUI deltas are "content moves" (natural); WebDriver wants page scroll amount.
                // Scale pane-space deltas to page CSS px so the page tracks the cursor.
                let scale = this.page_scale(i).map_or(1.0, |s| 1.0 / s);
                let (dx, dy) = (-f32::from(d.x) * scale, -f32::from(d.y) * scale);
                let Some((x, y)) = this.to_page(i, ev.position) else { return };
                if this.sync_input {
                    let rel = (x / this.panes[i].viewport.0.max(1.0), y / this.panes[i].viewport.1.max(1.0));
                    for p in &this.panes {
                        let (vw, vh) = p.viewport;
                        p.engine.send(Cmd::Scroll { x: rel.0 * vw, y: rel.1 * vh, dx, dy });
                    }
                } else {
                    this.panes[i].engine.send(Cmd::Scroll { x, y, dx, dy });
                }
            }))
            .child(
                canvas(move |b, _, _| bounds_cell.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            );

        let body = match (&pane.frame, &pane.status) {
            (Some(frame), _) => body.child(img(frame.clone()).size_full().object_fit(ObjectFit::Contain)),
            (None, Some(Status::Error(e))) => body.bg(rgb(0x1e1e2e)).child(error_view(kind, e)),
            (None, _) => body.bg(rgb(0x1e1e2e)).child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x6c7086))
                    .child(status_text.clone()),
            ),
        };
        // If an error happens after frames exist, overlay it.
        let body = match (&pane.frame, &pane.status) {
            (Some(_), Some(Status::Error(e))) => body.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .p_2()
                    .bg(rgb(0x45202a))
                    .text_xs()
                    .text_color(rgb(0xf38ba8))
                    .child(e.clone()),
            ),
            _ => body,
        };

        let title: SharedString = if pane.title.is_empty() { pane.url.clone() } else { pane.title.clone() }.into();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .rounded_md()
            .overflow_hidden()
            .border_2()
            .border_color(rgb(if focused { kind.accent() } else { 0x313244 }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .bg(rgb(0x181825))
                    .text_xs()
                    .child(div().size_2().rounded_full().bg(rgb(kind.accent())))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(0xcdd6f4))
                            .child(format!("{} · {}", kind.name(), kind.browser())),
                    )
                    .when(!pane.engine_version.is_empty(), |d| {
                        d.child(div().text_color(rgb(0x6c7086)).child(pane.engine_version.clone()))
                    })
                    .child(div().text_color(rgb(status_color)).child(status_text))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(0x9399b2))
                            .child(title),
                    )
                    .when(pane.viewport.0 > 0.0, |d| {
                        d.child(
                            div()
                                .text_color(rgb(0x6c7086))
                                .child(format!("{:.0}×{:.0}", pane.viewport.0, pane.viewport.1)),
                        )
                    }),
            )
            .child(body)
    }
}

impl Render for MultiBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("root")
            .on_key_down(cx.listener(Self::on_root_key))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x11111b))
            .text_color(rgb(0xcdd6f4))
            .child(self.render_toolbar(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap_2()
                            .child(self.render_pane(0, window, cx))
                            .child(self.render_pane(1, window, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap_2()
                            .child(self.render_pane(2, window, cx))
                            .child(self.render_pane(3, window, cx)),
                    ),
            )
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h_8()
        .min_w_8()
        .px_2()
        .rounded_md()
        .bg(rgb(0x313244))
        .hover(|s| s.bg(rgb(0x45475a)))
        .active(|s| s.bg(rgb(0x585b70)))
        .text_sm()
        .cursor_pointer()
        .child(label)
        .on_click(on_click)
}

fn error_view(kind: EngineKind, err: &str) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .p_6()
        .text_sm()
        .child(div().text_color(rgb(0xf38ba8)).child(format!("{} failed to start", kind.browser())))
        .child(div().text_xs().text_color(rgb(0xa6adc8)).child(err.to_string()))
}

fn normalize_url(input: &str) -> String {
    let s = input.trim();
    if s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        s.to_string()
    } else if !s.contains(' ') && (s.contains('.') || s.starts_with("localhost")) {
        if s.starts_with("localhost") || s.starts_with("127.") {
            format!("http://{s}")
        } else {
            format!("https://{s}")
        }
    } else {
        let q: String = s
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("https://duckduckgo.com/html/?q={q}")
    }
}

fn main() {
    // `--update [--check]`: update the engine runtimes in the data folder, then exit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("multibrowser {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.first().is_some_and(|a| a == "--update") {
        let check_only = args.iter().any(|a| a == "--check");
        if let Err(e) = runtime::update(check_only) {
            eprintln!("\nerror: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Finder may pass a process serial number (`-psn_…`) to apps it launches.
    let initial_url = args
        .first()
        .filter(|a| !a.starts_with("-psn_"))
        .map(|u| normalize_url(u))
        .unwrap_or(DEFAULT_URL.into());
    // Ctrl-C / SIGTERM: close browser sessions instead of orphaning them.
    let _ = ctrlc::set_handler(|| {
        engine::shutdown_all(Duration::from_secs(4));
        std::process::exit(0);
    });
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1600.), px(1000.)), cx);
        cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Wayland app id / X11 WM_CLASS: matches multibrowser.desktop, so the
                    // desktop shows the app's icon for this window.
                    app_id: Some("multibrowser".into()),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Multibrowser".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| MultiBrowser::new(initial_url.clone(), window, cx)),
            )
            .expect("open window");

        // Shut the browsers down cleanly when the app exits.
        cx.on_app_quit(|_| async { engine::shutdown_all(Duration::from_secs(4)) }).detach();
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
