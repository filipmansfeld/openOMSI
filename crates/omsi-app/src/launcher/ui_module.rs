//! Optional local UI modules. The engine draws native widgets; a private, headless
//! child owns accounts, downloads and device logic. No browser or foreign window is
//! embedded, and account tokens never form part of the control schema.

use super::theme::*;
use super::ui::{id_of, ButtonKind, Input, Key, Ui};
use glam::Vec2;
use omsi_render::{Renderer, Scene, TextureId};
use omsi_ui::paint::Align;
use omsi_ui::{Draw, Rect, Weight};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};

const MAX_FRAME: usize = 256 * 1024;
const POLL: Duration = Duration::from_millis(500);
const DEADLINE: Duration = Duration::from_secs(20);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MAX_RECONNECTS: u8 = 4;

#[derive(Clone, Copy)]
pub(crate) enum Context { Launcher, Game }
impl Context {
    fn name(self) -> &'static str { match self { Self::Launcher => "launcher", Self::Game => "game" } }
}

// Never Debug/Serialize this capability, nor put it in a duty, command line or file.
#[derive(Clone)]
struct Handoff { pipe_name: String, capability: String }
static HANDOFF: OnceLock<Mutex<Option<Handoff>>> = OnceLock::new();
static GAME_HANDED: AtomicBool = AtomicBool::new(false);
pub(crate) fn game_started() { GAME_HANDED.store(true, Ordering::Release); }

/// Only the game being launched receives the parent's RAM-only UI capability.
pub(crate) fn configure_game_environment(command: &mut Command) -> anyhow::Result<()> {
    command.env_remove("OMSI_UI_PIPE").env_remove("OMSI_UI_CAP");
    let handoff = HANDOFF.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(h) = handoff {
        command.env("OMSI_UI_PIPE", h.pipe_name).env("OMSI_UI_CAP", h.capability);
    } else if std::env::var_os("OMSI_UI_MODULE").is_some() {
        anyhow::bail!("The local UI module is still starting or unavailable. Wait for its status before starting the game.");
    }
    Ok(())
}

#[derive(Deserialize)]
struct Config {
    executable: PathBuf,
    #[serde(default)] args: Vec<String>,
    working_directory: Option<PathBuf>,
    sha256: Option<String>,
    #[serde(default)] dependencies: Vec<Pin>,
}
#[derive(Deserialize)]
struct Pin { path: PathBuf, sha256: String }
#[derive(Deserialize)]
struct Ready { v: u32, pipe_name: String, capability: String }
impl Ready {
    fn handoff(self) -> Result<Handoff, String> {
        if self.v != 1 || self.pipe_name.is_empty() || self.pipe_name.len() > 128
            || !self.pipe_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || self.capability.len() != 64 || !self.capability.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return Err("Invalid local UI module startup descriptor.".into());
        }
        Ok(Handoff { pipe_name: self.pipe_name, capability: self.capability })
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct Page { pub id: String, pub label: String }
#[derive(Clone, Deserialize, Default)]
struct Auth { #[serde(default)] state: String, #[serde(default)] display_name: String }
#[derive(Clone, Deserialize)]
struct OptionItem { id: String, label: String }
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind { Heading, Text, Status, Field, Choice, Button, Progress, Table }
#[derive(Clone, Deserialize)]
struct Item {
    kind: Kind, id: String,
    #[serde(default)] label: String,
    #[serde(default)] value: String,
    #[serde(default)] secret: bool,
    #[serde(default = "yes")] enabled: bool,
    action_id: Option<String>,
    #[serde(default)] options: Vec<OptionItem>,
    #[serde(default)] rows: Vec<Vec<String>>,
    fraction: Option<f32>, current: Option<u64>, total: Option<u64>,
}
fn yes() -> bool { true }
#[derive(Clone, Deserialize)]
struct State {
    revision: String, title: String, page: String, pages: Vec<Page>,
    #[serde(default)] auth: Auth,
    #[serde(default)] busy: bool,
    items: Vec<Item>,
}
impl State {
    fn validate(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        if self.revision.is_empty() || self.revision.len() > 128 || self.title.len() > 256
            || self.pages.len() > 16 || self.items.len() > 256 || self.auth.display_name.len() > 256
            || self.auth.state.len() > 64 || !self.pages.iter().any(|p| p.id == self.page) {
            return Err("Invalid local UI state.".into());
        }
        for page in &self.pages {
            if !valid_id(&page.id) || page.label.len() > 256 || !ids.insert(page.id.as_str()) {
                return Err("Invalid or duplicate local UI page.".into());
            }
        }
        ids.clear();
        for item in &self.items {
            if !valid_id(&item.id) || !ids.insert(item.id.as_str()) || item.label.len() > 4096
                || item.value.len() > 4096 || item.options.len() > 512 || item.rows.len() > 2048
                || item.action_id.as_ref().is_some_and(|x| !valid_id(x))
                || item.fraction.is_some_and(|x| !x.is_finite() || !(0.0..=1.0).contains(&x))
                || item.secret && (!matches!(item.kind, Kind::Field) || !item.value.is_empty()) {
                return Err("Invalid local UI control.".into());
            }
            let mut options = HashSet::new();
            if item.options.iter().any(|o| !valid_id(&o.id) || o.label.len() > 512 || !options.insert(o.id.as_str()))
                || item.rows.iter().any(|r| r.len() > 16 || r.iter().any(|c| c.len() > 4096)) {
                return Err("Invalid local UI choices or table.".into());
            }
        }
        Ok(())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}
fn cap_value(value: &mut String) {
    if value.len() > 4096 {
        let mut end = 4096;
        while !value.is_char_boundary(end) { end -= 1; }
        value.truncate(end);
    }
}
#[derive(Serialize)]
struct Request {
    v: u32, id: String, op: &'static str, params: Params,
    #[serde(skip_serializing_if = "Option::is_none")] capability: Option<String>,
}
#[derive(Serialize)]
struct Params {
    context: &'static str, page: String,
    #[serde(skip_serializing_if = "Option::is_none")] engine_pid: Option<u32>,
    // Private server ticket for the headless module, never a UI control value.
    #[serde(skip_serializing_if = "Option::is_none")] mp_authorization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] action_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] revision: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")] values: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct Reply { v: u32, id: String, ok: bool, state: Option<State>, error: Option<String> }
enum Event { Ready, Reply(Reply), Failed(String) }

fn read_json<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> Result<T, String> {
    let mut length = [0; 4];
    reader.read_exact(&mut length).map_err(|_| "Local UI module connection ended.".to_string())?;
    let size = u32::from_le_bytes(length) as usize;
    if size == 0 || size > MAX_FRAME { return Err("Invalid local UI frame length.".into()); }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).map_err(|_| "Incomplete local UI frame.".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid local UI JSON frame.".into())
}
fn write_json(writer: &mut impl Write, request: &Request) -> Result<(), String> {
    let bytes = serde_json::to_vec(request).map_err(|_| "Invalid local UI request.".to_string())?;
    if bytes.len() > MAX_FRAME { return Err("Local UI request is too large.".into()); }
    writer.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| writer.write_all(&bytes))
        .and_then(|_| writer.flush()).map_err(|_| "Local UI module connection ended.".into())
}
fn verify_pin(path: &Path, expected: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) { return Err("Invalid local UI dependency hash.".into()); }
    let mut file = std::fs::File::open(path).map_err(|_| "A local UI dependency is missing.".to_string())?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|_| "Cannot read a local UI dependency.".to_string())?;
        if n == 0 { break; }
        hash.update(&buf[..n]);
    }
    if format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase() { return Err("A local UI dependency does not match its approved hash.".into()); }
    Ok(())
}

#[derive(Clone)]
enum Origin { Child(PathBuf), Pipe(Handoff) }
struct Transport {
    tx: mpsc::SyncSender<Request>, rx: mpsc::Receiver<Event>,
    stopped: Arc<AtomicBool>, child: Arc<Mutex<Option<Child>>>,
}
impl Transport {
    fn start(origin: Origin) -> Self {
        let (tx, requests) = mpsc::sync_channel::<Request>(1);
        let (events, rx) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let stop = stopped.clone(); let process = child.clone();
        std::thread::spawn(move || {
            let run = || -> Result<(), String> {
                let (mut input, mut output, capability): (Box<dyn Write>, Box<dyn Read>, Option<String>) = match origin {
                    Origin::Child(path) => {
                        let mut bytes = Vec::new();
                        std::fs::File::open(&path).map_err(|_| "Cannot read the local UI module configuration.".to_string())?
                            .take(MAX_FRAME as u64 + 1).read_to_end(&mut bytes).map_err(|_| "Cannot read the local UI module configuration.".to_string())?;
                        if bytes.len() > MAX_FRAME { return Err("Local UI configuration is too large.".into()); }
                        let config: Config = serde_json::from_slice(&bytes).map_err(|_| "Invalid local UI module configuration.".to_string())?;
                        if !config.executable.is_absolute() || config.args.len() > 32 || config.args.iter().any(|x| x.len() > 4096) || config.dependencies.len() > 128 {
                            return Err("Local UI executable must be an explicit absolute path.".into());
                        }
                        let directory = config.working_directory.unwrap_or_else(|| config.executable.parent().unwrap_or(Path::new(".")).to_path_buf());
                        if !directory.is_absolute() { return Err("Local UI working directory must be absolute.".into()); }
                        if let Some(hash) = &config.sha256 { verify_pin(&config.executable, hash)?; }
                        for pin in &config.dependencies { verify_pin(&if pin.path.is_absolute() { pin.path.clone() } else { directory.join(&pin.path) }, &pin.sha256)?; }
                        let mut command = Command::new(config.executable);
                        command.args(config.args).current_dir(directory).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
                        #[cfg(windows)] {
                            use std::os::windows::process::CommandExt;
                            command.creation_flags(0x0800_0000);
                        }
                        if stop.load(Ordering::Acquire) { return Ok(()); }
                        let mut spawned = command.spawn().map_err(|_| "Cannot start the local UI module.".to_string())?;
                        let input = spawned.stdin.take().ok_or("Local UI input pipe is missing.")?;
                        let mut output = spawned.stdout.take().ok_or("Local UI output pipe is missing.")?;
                        *process.lock().unwrap_or_else(|e| e.into_inner()) = Some(spawned);
                        if stop.load(Ordering::Acquire) {
                            if let Some(mut child) = process.lock().unwrap_or_else(|e| e.into_inner()).take() { let _ = child.kill(); let _ = child.wait(); }
                            return Ok(());
                        }
                        let h = read_json::<Ready>(&mut output)?.handoff()?;
                        if stop.load(Ordering::Acquire) { return Ok(()); }
                        *HANDOFF.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                        GAME_HANDED.store(false, Ordering::Release);
                        (Box::new(input), Box::new(output), None)
                    }
                    Origin::Pipe(h) => {
                        #[cfg(windows)] {
                            let file = std::fs::OpenOptions::new().read(true).write(true).open(format!("\\\\.\\pipe\\{}", h.pipe_name))
                                .map_err(|_| "Cannot connect to the parent UI module.".to_string())?;
                            let input = file.try_clone().map_err(|_| "Cannot open the parent UI module pipe.".to_string())?;
                            (Box::new(input), Box::new(file), Some(h.capability))
                        }
                        #[cfg(not(windows))] { let _ = h; return Err("Local UI named-pipe handoff is only available on Windows.".into()); }
                    }
                };
                if events.send(Event::Ready).is_err() { return Ok(()); }
                while let Ok(mut request) = requests.recv() {
                    if stop.load(Ordering::Acquire) { break; }
                    request.capability = capability.clone();
                    write_json(&mut input, &request)?;
                    let reply: Reply = read_json(&mut output)?;
                    if reply.v != 1 || reply.id != request.id { return Err("Mismatched local UI reply.".into()); }
                    if let Some(state) = &reply.state { state.validate()?; }
                    if events.send(Event::Reply(reply)).is_err() { break; }
                }
                Ok(())
            };
            if let Err(error) = run() { let _ = events.send(Event::Failed(error)); }
            stop.store(true, Ordering::Release);
        });
        Self { tx, rx, stopped, child }
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            *HANDOFF.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = None;
            if !GAME_HANDED.load(Ordering::Acquire) { std::thread::spawn(move || { let _ = child.kill(); let _ = child.wait(); }); }
            // After a successful handoff, the backend owns its bounded stdin-EOF
            // grace and stays alive until its final authenticated game pipe closes.
        }
    }
}
impl Drop for Transport { fn drop(&mut self) { self.stop(); } }

pub(crate) struct Module {
    transport: Transport, context: Context, pub page: String, state: Option<State>,
    values: BTreeMap<String, String>, ready: bool, failed: bool,
    next: Instant, pending: Option<Instant>, sequence: u64, error: Option<String>,
    pending_action: bool, queued_action: Option<Request>,
    request_prefix: String,
    reconnect_origin: Option<Origin>, reconnect_at: Option<Instant>, reconnects: u8,
    action_unknown: bool,
}
impl Module {
    pub(crate) fn from_env(context: Context) -> Option<Self> {
        let origin = if matches!(context, Context::Game) {
            match (std::env::var("OMSI_UI_PIPE"), std::env::var("OMSI_UI_CAP")) {
                (Ok(pipe_name), Ok(capability)) => Origin::Pipe(Ready { v: 1, pipe_name, capability }.handoff().ok()?),
                _ => {
                    let h = HANDOFF.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if let Some(h) = h { Origin::Pipe(h) } else { Origin::Child(std::env::var_os("OMSI_UI_MODULE")?.into()) }
                }
            }
        } else { Origin::Child(std::env::var_os("OMSI_UI_MODULE")?.into()) };
        let reconnect_origin = matches!(context, Context::Game).then(|| match &origin {
            Origin::Pipe(_) => Some(origin.clone()), _ => None,
        }).flatten();
        Some(Self { transport: Transport::start(origin), context, page: "downloads".into(), state: None,
            values: BTreeMap::new(), ready: false, failed: false, next: Instant::now(),
            pending: Some(Instant::now()), sequence: 0, error: None,
            pending_action: false, queued_action: None, reconnect_origin, reconnect_at: None, reconnects: 0, action_unknown: false,
            request_prefix: format!("{}-{:016x}", std::process::id(), rand::random::<u64>()) })
    }
    pub(crate) fn title(&self) -> &str { self.state.as_ref().map(|s| s.title.as_str()).unwrap_or("Extensions") }
    pub(crate) fn pages(&self) -> Vec<Page> { self.state.as_ref().map(|s| s.pages.clone()).unwrap_or_default() }
    pub(crate) fn select_page(&mut self, page: &str) {
        if self.page != page && valid_id(page) && (self.state.is_none() || self.pages().iter().any(|p| p.id == page)) {
            self.clear_secrets(); self.values.clear(); self.queued_action = None; self.page = page.into(); self.next = Instant::now();
        }
    }
    pub(crate) fn clear_secrets(&mut self) {
        if let Some(state) = &self.state { for item in &state.items { if item.secret { self.values.remove(&item.id); } } }
    }
    pub(crate) fn tick(&mut self, visible: bool) {
        if self.reconnect_at.is_some_and(|at| at <= Instant::now()) {
            if let Some(origin) = &self.reconnect_origin {
                self.replace_read_transport(Transport::start(origin.clone()));
            }
            self.reconnect_at = None;
        }
        while let Ok(event) = self.transport.rx.try_recv() {
            match event {
                Event::Ready => { self.ready = true; self.pending = None; self.pending_action = false; }
                Event::Reply(reply) => {
                    let valid_state_reply = reply.ok && reply.state.is_some();
                    self.pending = None;
                    self.pending_action = false;
                    self.error = reply.error.filter(|e| !e.is_empty()).map(|e| e.chars().take(2048).collect());
                    if self.action_unknown && self.error.is_none() {
                        self.error = Some("The previous action was not confirmed. Review its status before submitting again.".into());
                    }
                    if let Some(state) = reply.state {
                        if self.state.as_ref().is_some_and(|s| s.auth.state != state.auth.state || s.auth.display_name != state.auth.display_name) {
                            self.clear_secrets();
                            if self.queued_action.take().is_some() {
                                self.error = Some("The account state changed. Review the form before submitting again.".into());
                            }
                        }
                        if state.page == self.page {
                            let retained: HashSet<_> = state.items.iter().filter(|i| matches!(i.kind, Kind::Field | Kind::Choice)).map(|i| i.id.as_str()).collect();
                            self.values.retain(|id, _| retained.contains(id.as_str()));
                            for item in &state.items { if matches!(item.kind, Kind::Field | Kind::Choice) { self.values.entry(item.id.clone()).or_insert_with(|| if item.secret { String::new() } else { item.value.clone() }); } }
                            self.state = Some(state);
                        }
                    }
                    if !reply.ok && self.error.is_none() { self.error = Some("The local UI action was rejected.".into()); }
                    if valid_state_reply { self.reconnects = 0; }
                }
                Event::Failed(error) => { self.fail(); if !self.action_unknown { self.error = Some(error); } }
            }
        }
        if self.pending.is_some_and(|t| t.elapsed() >= DEADLINE) { self.fail(); }
        if !self.failed && self.ready && self.pending.is_none() {
            if let Some(request) = self.queued_action.take() { self.submit(request); }
        }
        if !self.failed && self.ready && self.pending.is_none() && self.next <= Instant::now()
            && (visible || self.state.is_none() || matches!(self.context, Context::Game)) {
            self.send(None);
            if !visible && self.state.is_some() { self.next = Instant::now() + Duration::from_secs(2); }
        }
    }
    fn fail(&mut self) {
        self.action_unknown |= self.pending_action;
        self.failed = true; self.ready = false; self.pending = None; self.pending_action = false;
        self.queued_action = None; self.clear_secrets();
        self.error = Some("The local UI module stopped responding. No action will be retried automatically.".into());
        self.transport.stop();
        if self.reconnect_origin.is_some() && self.reconnects < MAX_RECONNECTS && self.reconnect_at.is_none() {
            self.reconnects += 1;
            self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
        }
    }
    fn replace_read_transport(&mut self, transport: Transport) {
        self.transport = transport;
        self.failed = false; self.ready = false; self.pending_action = false;
        self.queued_action = None; self.clear_secrets();
        self.pending = Some(Instant::now()); self.next = Instant::now();
    }
    fn retry_read_connection(&mut self) {
        if self.failed && self.reconnect_origin.is_some() && self.reconnect_at.is_none() {
            self.reconnects = 0;
            self.reconnect_at = Some(Instant::now());
        }
    }
    fn send(&mut self, action_id: Option<String>) {
        if !self.ready || self.failed || self.queued_action.is_some()
            || self.pending.is_some() && (action_id.is_none() || self.pending_action) { return; }
        self.sequence += 1;
        let action = action_id.is_some();
        if action { self.action_unknown = false; }
        let request = Request { v: 1, id: format!("{}-{}", self.request_prefix, self.sequence), op: if action { "ui.action" } else { "ui.get" }, capability: None,
            params: Params { context: self.context.name(), page: self.page.clone(), action_id,
                engine_pid: matches!(self.context, Context::Game).then(std::process::id),
                mp_authorization: if matches!(self.context, Context::Game) {
                    omsi_net::access::token_for(super::core::TANGENTA_ACCESS_ORIGIN).ok().flatten()
                } else { None },
                revision: self.state.as_ref().filter(|s| s.page == self.page).map(|s| s.revision.clone()),
                values: if action { self.values.clone() } else { BTreeMap::new() } } };
        if serde_json::to_vec(&request).map_or(true, |bytes| bytes.len() > MAX_FRAME) {
            self.error = Some("The form is too large to submit.".into()); self.clear_secrets(); return;
        }
        // A background refresh must neither make buttons disappear nor eat a click.
        // Hold one explicit action until that read finishes, retaining the displayed
        // revision so the backend still rejects a form that has since changed.
        if self.pending.is_some() { self.queued_action = Some(request); }
        else { self.submit(request); }
        if action { self.clear_secrets(); }
    }
    fn submit(&mut self, request: Request) {
        let action = request.op == "ui.action";
        if self.transport.tx.try_send(request).is_ok() {
            self.pending = Some(Instant::now()); self.next = Instant::now() + POLL;
            self.pending_action = action;
        } else { self.fail(); }
    }
    /// Native controls on the same toolkit and theme as all other launcher pages.
    pub(crate) fn draw(&mut self, ui: &mut Ui, area: Rect) {
        self.draw_inner(ui, area, true);
    }
    pub(crate) fn draw_page(&mut self, ui: &mut Ui, area: Rect, page: &str) {
        self.select_page(page);
        self.draw_inner(ui, area, false);
    }
    fn draw_inner(&mut self, ui: &mut Ui, area: Rect, tabs: bool) {
        self.tick(true);
        cap_value(&mut ui.input.text);
        if let Some(value) = ui.clipboard_in.as_mut() { cap_value(value); }
        ui.panel(area);
        let body = area.pad(20.0, 16.0);
        let mut y = body.y;
        ui.text_in(self.title(), Rect::new(body.x, y, body.w, 32.0), 23.0, Weight::Bold, TEXT, Align::Left); y += 40.0;
        let pages = self.pages();
        if tabs && !pages.is_empty() {
            let labels: Vec<_> = pages.iter().map(|p| p.label.as_str()).collect();
            let mut selected = pages.iter().position(|p| p.id == self.page).unwrap_or(0);
            if ui.segmented("module-pages", Rect::new(body.x, y, body.w, ROW), &mut selected, &labels) { self.select_page(&pages[selected].id); }
            y += ROW + GAP;
        }
        if let Some(error) = &self.error { y += ui.paragraph(error, Vec2::new(body.x, y), body.w, 13.0, Weight::Medium, DANGER) + GAP; }
        let Some(state) = self.state.clone().filter(|s| s.page == self.page) else {
            ui.label(Rect::new(body.x, y, body.w, ROW), if self.failed { "Local module unavailable" } else { "Loading…" }); return;
        };
        if !state.auth.display_name.is_empty() { ui.label(Rect::new(body.x, y, body.w, ROW), &state.auth.display_name); y += ROW; }
        if state.busy { ui.label(Rect::new(body.x, y, body.w, 24.0), "Working…"); y += 28.0; }
        let enabled = !self.failed && !self.pending_action && self.queued_action.is_none();
        let mut action = None;
        ui.scroll_area("module-body", Rect::new(body.x, y, body.w, (body.bottom() - y).max(1.0)), &mut |ui, r| {
            let mut y = r.y;
            for item in &state.items {
                let name = format!("module-{}-{}", state.page, item.id);
                match item.kind {
                    Kind::Heading => { ui.text_in(&item.label, Rect::new(r.x, y, r.w, 32.0), 18.0, Weight::Bold, TEXT, Align::Left); y += 40.0; }
                    Kind::Text | Kind::Status => { y += ui.paragraph(&format!("{}{}{}", item.label, if item.value.is_empty() { "" } else { "\n" }, item.value), Vec2::new(r.x, y), r.w, 13.0, Weight::Medium, TEXT_SOFT) + GAP; }
                    Kind::Field => {
                        ui.label(Rect::new(r.x, y, r.w, 24.0), &item.label); y += 26.0;
                        let value = self.values.entry(item.id.clone()).or_default();
                        if item.enabled {
                            if item.secret { ui.password_input(&name, Rect::new(r.x, y, r.w.min(620.0), ROW), value, ""); }
                            else { ui.text_input(&name, Rect::new(r.x, y, r.w.min(620.0), ROW), value, "", None); }
                            cap_value(value);
                        } else { ui.label(Rect::new(r.x, y, r.w, ROW), if item.secret { "" } else { value }); }
                        y += ROW + GAP;
                    }
                    Kind::Choice => {
                        ui.label(Rect::new(r.x, y, r.w, 24.0), &item.label); y += 26.0;
                        let value = self.values.entry(item.id.clone()).or_default();
                        let mut selected = item.options.iter().position(|o| o.id == *value).map(|i| i + 1).unwrap_or(0);
                        let labels: Vec<_> = std::iter::once("Select…".to_string()).chain(item.options.iter().map(|o| o.label.clone())).collect();
                        if item.enabled && !item.options.is_empty() && ui.select(&name, Rect::new(r.x, y, r.w.min(620.0), ROW), &mut selected, &labels) {
                            *value = selected.checked_sub(1).and_then(|i| item.options.get(i)).map(|o| o.id.clone()).unwrap_or_default();
                        }
                        y += ROW + GAP;
                    }
                    Kind::Button => {
                        let rect = Rect::new(r.x, y, r.w.min(620.0), ROW);
                        if ui.button_enabled(&name, rect, &item.label, None, ButtonKind::Normal,
                            enabled && item.enabled && item.action_id.is_some()) { action = item.action_id.clone(); }
                        y += ROW + GAP;
                    }
                    Kind::Progress => {
                        let label = match (item.current, item.total) { (Some(c), Some(t)) => format!("{}  {c}/{t}", item.label), _ => item.label.clone() };
                        ui.label(Rect::new(r.x, y, r.w, 24.0), &label); y += 28.0;
                        ui.progress(Rect::new(r.x, y, r.w, 8.0), item.fraction.unwrap_or(0.0), item.fraction.is_none()); y += 8.0 + GAP;
                    }
                    Kind::Table => {
                        ui.label(Rect::new(r.x, y, r.w, 24.0), &item.label); y += 28.0;
                        for row in &item.rows {
                            let width = r.w / row.len().max(1) as f32;
                            for (i, cell) in row.iter().enumerate() { ui.text_in(cell, Rect::new(r.x + i as f32 * width, y, width - 8.0, 28.0), 12.0, Weight::Regular, TEXT_SOFT, Align::Left); }
                            y += 28.0;
                        }
                        y += GAP;
                    }
                }
            }
            y - r.y
        });
        if let Some(action) = action { self.send(Some(action)); }
    }
}

/// The game's native control panel uses the launcher's widget toolkit and the existing
/// renderer's overlay texture, rather than a capture of another application's window.
pub(crate) struct GamePanel {
    module: Module, ui: Ui, gpu: Option<omsi_ui::Gpu>, target: Option<(TextureId, u32, u32)>,
    pub open: bool, modifiers: winit::keyboard::ModifiersState,
    #[cfg(not(target_os = "android"))] clipboard: Option<arboard::Clipboard>,
}
impl GamePanel {
    pub(crate) fn from_env() -> Option<Self> {
        let mut module = Module::from_env(Context::Game)?;
        module.select_page("profile");
        Some(Self { module, ui: Ui::new(), gpu: None, target: None, open: false, modifiers: Default::default(),
            #[cfg(not(target_os = "android"))] clipboard: arboard::Clipboard::new().ok() })
    }
    pub(crate) fn toggle(&mut self) {
        self.open = !self.open;
        if self.open { self.module.next = Instant::now(); self.module.retry_read_connection(); }
        else { self.ui.focus = None; self.module.clear_secrets(); self.ui.discard_input(); }
    }
    pub(crate) fn drop_gpu(&mut self) { self.gpu = None; self.target = None; }
    pub(crate) fn tick(&mut self) { self.module.tick(self.open); }
    pub(crate) fn input(&mut self, event: &WindowEvent, scale: f32) -> bool {
        if !self.open { return false; }
        let input = &mut self.ui.input;
        match event {
            WindowEvent::CursorMoved { position, .. } => input.mouse = Vec2::new(position.x as f32, position.y as f32) / scale.max(0.1),
            WindowEvent::MouseInput { state, button, .. } => {
                let down = *state == ElementState::Pressed;
                if *button == MouseButton::Left { input.down = down; input.pressed |= down; input.released |= !down; }
                else if *button == MouseButton::Right { input.right_down = down; input.right_pressed |= down; }
            }
            WindowEvent::MouseWheel { delta, .. } => { input.wheel += match delta { MouseScrollDelta::LineDelta(x, y) => Vec2::new(*x, *y) * 40.0, MouseScrollDelta::PixelDelta(p) => Vec2::new(p.x as f32, p.y as f32) / scale.max(0.1) }; }
            WindowEvent::ModifiersChanged(m) => { self.modifiers = m.state(); input.shift = self.modifiers.shift_key(); input.ctrl = self.modifiers.control_key(); input.alt = self.modifiers.alt_key(); }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        if code == KeyCode::Escape { self.toggle(); return true; }
                        let command = self.modifiers.control_key() || self.modifiers.super_key();
                        let key = match code {
                            KeyCode::ArrowLeft => Some(Key::Left), KeyCode::ArrowRight => Some(Key::Right), KeyCode::ArrowUp => Some(Key::Up), KeyCode::ArrowDown => Some(Key::Down),
                            KeyCode::Home => Some(Key::Home), KeyCode::End => Some(Key::End), KeyCode::Backspace => Some(Key::Backspace), KeyCode::Delete => Some(Key::Delete),
                            KeyCode::Enter | KeyCode::NumpadEnter => Some(Key::Enter), KeyCode::Tab => Some(Key::Tab),
                            KeyCode::KeyA if command => Some(Key::SelectAll), KeyCode::KeyC if command => Some(Key::Copy), KeyCode::KeyX if command => Some(Key::Cut),
                            KeyCode::KeyV if command => Some(Key::Paste),
                            _ => None,
                        };
                        if let Some(key) = key {
                            #[cfg(not(target_os = "android"))]
                            if key == Key::Paste { self.ui.clipboard_in = self.clipboard.as_mut().and_then(|c| c.get_text().ok()); }
                            input.keys.push(key);
                        }
                        if !command { if let Some(text) = &event.text { input.text.push_str(text); } }
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => input.text.push_str(text),
            WindowEvent::Focused(false) => { self.ui.input = Input::default(); self.ui.focus = None; return false; }
            WindowEvent::Touch(t) => {
                input.touch = true; input.mouse = Vec2::new(t.location.x as f32, t.location.y as f32) / scale.max(0.1);
                match t.phase { winit::event::TouchPhase::Started => { input.down = true; input.pressed = true; }, winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled => { input.down = false; input.released = true; }, _ => {} }
            }
            _ => return false,
        }
        true
    }
    pub(crate) fn draw(&mut self, renderer: &Renderer, scene: &mut Scene, w: u32, h: u32, scale: f32, dt: f32) {
        self.module.tick(self.open);
        if !self.open || w == 0 || h == 0 { return; }
        if self.target.is_none_or(|(_, tw, th)| tw != w || th != h) {
            let old = self.target.take().map(|(id, _, _)| id);
            let added = renderer.add_render_texture(scene, w, h);
            let id = old.map(|id| renderer.recycle_texture(scene, added, id)).unwrap_or(added);
            self.target = Some((id, w, h));
        }
        let (id, _, _) = self.target.unwrap();
        let Some(view) = renderer.texture_view(scene, id) else { self.drop_gpu(); return; };
        let scale = scale.max(0.1);
        let size = Vec2::new(w as f32, h as f32) / scale;
        self.ui.begin(size, scale, dt);
        let area = Rect::new(16.0, 16.0, (size.x - 32.0).max(1.0), (size.y - 32.0).max(1.0));
        self.module.draw(&mut self.ui, Rect::new(area.x, area.y, area.w, (area.h - 48.0).max(1.0)));
        #[cfg(not(target_os = "android"))]
        if let Some(text) = self.ui.clipboard_out.take() { if let Some(clipboard) = self.clipboard.as_mut() { let _ = clipboard.set_text(text); } }
        if self.ui.button("module-close", Rect::new(area.x, area.bottom() - 40.0, 160.0, ROW), "Close", None, ButtonKind::Normal) { self.toggle(); }
        let (layers, vertices, ranges) = self.ui.finish();
        let gpu = self.gpu.get_or_insert_with(|| omsi_ui::Gpu::new(&renderer.device, renderer.format(), 1, 2048));
        gpu.upload(&renderer.device, &renderer.queue, 0, &vertices);
        gpu.upload_atlas(&renderer.queue, &mut self.ui.atlas);
        let draws: Vec<_> = ranges.into_iter().enumerate().map(|(layer, (range, texture))| Draw { buffer: 0, range, layer, texture }).collect();
        let mut encoder = renderer.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("native module controls") });
        gpu.render(&renderer.device, &renderer.queue, &mut encoder, &view, (w, h), Some(wgpu::Color::TRANSPARENT), &layers, &draws);
        renderer.queue.submit([encoder.finish()]);
        if self.open { scene.overlays.push((id, [0.0, 0.0, w as f32, h as f32])); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_fixture() -> (Module, mpsc::Receiver<Request>, mpsc::SyncSender<Event>) {
        let (tx, requests) = mpsc::sync_channel(1);
        let (events, rx) = mpsc::sync_channel(1);
        let state: State = serde_json::from_str(r#"{"revision":"1","title":"Module","page":"downloads","pages":[{"id":"downloads","label":"Downloads"},{"id":"profile","label":"Profile"}],"auth":{"state":"verified","display_name":"Fixture"},"items":[{"kind":"choice","id":"product","options":[{"id":"fixture","label":"Fixture"}]},{"kind":"button","id":"install","action_id":"downloads.install"}]}"#).unwrap();
        let module = Module {
            transport: Transport { tx, rx, stopped: Arc::new(AtomicBool::new(false)), child: Arc::new(Mutex::new(None)) },
            context: Context::Launcher, page: "downloads".into(), state: Some(state),
            values: BTreeMap::from([("product".into(), "fixture".into())]), ready: true, failed: false,
            next: Instant::now() + POLL, pending: None, sequence: 0, error: None,
            pending_action: false, queued_action: None, request_prefix: "fixture".into(),
            reconnect_origin: None, reconnect_at: None, reconnects: 0, action_unknown: false,
        };
        (module, requests, events)
    }

    #[test]
    fn a_click_during_a_refresh_is_submitted_once_with_its_displayed_values() {
        let (mut module, requests, events) = module_fixture();
        module.send(None);
        let poll = requests.try_recv().unwrap();
        assert!(!module.pending_action);
        module.send(Some("downloads.install".into()));
        module.values.insert("product".into(), "changed-later".into());
        module.send(Some("downloads.refresh".into()));
        assert!(requests.try_recv().is_err());
        let mut refreshed = module.state.clone().unwrap();
        refreshed.revision = "2".into();
        events.send(Event::Reply(Reply { v: 1, id: poll.id, ok: true, state: Some(refreshed), error: None })).unwrap();
        module.tick(false);
        let action = requests.try_recv().unwrap();
        assert_eq!(action.op, "ui.action");
        assert_eq!(action.params.action_id.as_deref(), Some("downloads.install"));
        assert_eq!(action.params.revision.as_deref(), Some("1"));
        assert_eq!(action.params.values.get("product").map(String::as_str), Some("fixture"));
        assert!(module.pending_action);
        // A rejection of that stale form is displayed, never resubmitted.
        events.send(Event::Reply(Reply { v: 1, id: action.id, ok: false, state: module.state.clone(), error: Some("State changed".into()) })).unwrap();
        module.tick(false);
        assert!(requests.try_recv().is_err());
        assert!(module.queued_action.is_none());
        assert_eq!(module.error.as_deref(), Some("State changed"));
    }

    #[test]
    fn changing_page_account_or_transport_discards_a_queued_action() {
        for reason in ["page", "account", "transport"] {
            let (mut module, requests, events) = module_fixture();
            module.send(None);
            let poll = requests.try_recv().unwrap();
            module.send(Some("downloads.install".into()));
            assert!(module.queued_action.is_some());
            match reason {
                "page" => {
                    module.select_page("profile");
                    events.send(Event::Reply(Reply { v: 1, id: poll.id, ok: true, state: module.state.clone(), error: None })).unwrap();
                }
                "account" => {
                    let mut changed = module.state.clone().unwrap();
                    changed.auth.state = "signed_out".into();
                    events.send(Event::Reply(Reply { v: 1, id: poll.id, ok: true, state: Some(changed), error: None })).unwrap();
                }
                _ => { events.send(Event::Failed("Connection ended".into())).unwrap(); }
            }
            module.tick(false);
            assert!(module.queued_action.is_none());
            while let Ok(request) = requests.try_recv() { assert_eq!(request.op, "ui.get"); }
        }
    }

    #[test]
    fn bounded_frames_reject_length_and_mismatched_shape() {
        assert!(read_json::<Reply>(&mut std::io::Cursor::new((MAX_FRAME as u32 + 1).to_le_bytes())).is_err());
        let bytes = br#"{"v":1,"id":"7","ok":true,"state":null}"#;
        let mut frame = (bytes.len() as u32).to_le_bytes().to_vec(); frame.extend(bytes);
        let reply = read_json::<Reply>(&mut std::io::Cursor::new(frame)).unwrap();
        assert_eq!(reply.id, "7"); assert!(reply.ok);
    }
    #[test]
    fn game_read_reconnect_never_replays_an_unconfirmed_action_or_stale_form() {
        let (mut module, requests, events) = module_fixture();
        module.context = Context::Game;
        module.send(Some("downloads.install".into()));
        let action = requests.try_recv().unwrap();
        events.send(Event::Failed("Connection ended".into())).unwrap();
        module.tick(false);
        assert!(module.action_unknown && module.failed);
        assert!(module.queued_action.is_none());
        let (tx, fresh_requests) = mpsc::sync_channel(1);
        let (fresh_events, rx) = mpsc::sync_channel(1);
        module.replace_read_transport(Transport { tx, rx, stopped: Arc::new(AtomicBool::new(false)), child: Arc::new(Mutex::new(None)) });
        fresh_events.send(Event::Ready).unwrap();
        module.tick(false);
        let poll = fresh_requests.try_recv().unwrap();
        assert_eq!(poll.op, "ui.get");
        assert_ne!(poll.id, action.id);
        assert!(poll.params.action_id.is_none() && poll.params.values.is_empty());
        let mut stale = module.state.clone().unwrap();
        stale.page = "profile".into(); stale.revision = "stale".into();
        fresh_events.send(Event::Reply(Reply { v: 1, id: poll.id, ok: true, state: Some(stale), error: None })).unwrap();
        module.reconnects = 3;
        module.tick(false);
        assert_eq!(module.state.as_ref().unwrap().revision, "1");
        assert!(module.action_unknown && module.error.as_ref().unwrap().contains("not confirmed"));
        assert!(fresh_requests.try_recv().is_err());
        assert_eq!(module.reconnects, 0);
        module.reconnect_origin = Some(Origin::Pipe(Handoff { pipe_name: "fixture".into(), capability: "a".repeat(64) }));
        module.reconnects = MAX_RECONNECTS;
        module.fail();
        assert!(module.reconnect_at.is_none());
        module.retry_read_connection();
        assert!(module.reconnect_at.is_some() && module.action_unknown);
        assert!(module.queued_action.is_none());
    }
    #[test]
    fn native_control_state_rejects_duplicate_ids_and_returned_passwords() {
        let fixture = r#"{"revision":"1","title":"Module","page":"profile","pages":[{"id":"profile","label":"Account"}],"items":[{"kind":"field","id":"password","label":"Password","secret":true}]}"#;
        let mut state: State = serde_json::from_str(fixture).unwrap(); assert!(state.validate().is_ok());
        state.items[0].value = "must-not-return".into(); assert!(state.validate().is_err());
        state.items[0].value.clear(); state.items.push(state.items[0].clone()); assert!(state.validate().is_err());
    }
    #[test]
    fn handoff_rejects_non_pipe_paths_and_bad_capabilities() {
        assert!(Ready { v: 1, pipe_name: "../file".into(), capability: "a".repeat(64) }.handoff().is_err());
        assert!(Ready { v: 1, pipe_name: "OpenOmsiUi-test".into(), capability: "a".repeat(64) }.handoff().is_ok());
        assert!(Ready { v: 1, pipe_name: "OpenOmsiUi-test".into(), capability: "a".repeat(63) }.handoff().is_err());
    }

    #[test]
    fn multiplayer_authorization_is_private_and_absent_from_launcher_requests() {
        let (mut module, requests, _) = module_fixture();
        module.send(None);
        let mut request = requests.try_recv().unwrap();
        let launcher = serde_json::to_value(&request).unwrap();
        assert!(launcher["params"].get("mp_authorization").is_none());
        assert!(launcher["params"]["values"].get("mp_authorization").is_none());
        request.params.context = "game";
        request.params.engine_pid = Some(1);
        request.params.mp_authorization = Some("synthetic-test-ticket".into());
        let game = serde_json::to_value(&request).unwrap();
        assert_eq!(game["params"]["mp_authorization"], "synthetic-test-ticket");
        assert!(game["params"]["values"].get("mp_authorization").is_none());
        assert!(game.get("mp_authorization").is_none());
    }
}
