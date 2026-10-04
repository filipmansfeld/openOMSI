//! Opt-in local API underpinning the external plugin compatibility backend.
//! This is a capability-based API, not an emulation of OMSI's process memory layout.
//! The socket threads only read immutable
//! snapshots and enqueue commands; game state and GPU resources belong to the frame thread.

use crate::App;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

mod command;
pub(crate) mod protocol;
use crate::plugin_api::random_id;
use command::{Receipt, ReplayGuard, State};
use protocol::{authenticated, read_frame, write_response, Request};
const MAX_CLIENTS: usize = 4;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

fn audio_args(receipt: &Value) -> Value {
    let mut args = serde_json::Map::new();
    for key in ["id", "generation", "session_id", "section", "audio_generation", "lease"] {
        if let Some(value) = receipt.get(key) {
            args.insert(key.into(), value.clone());
        }
    }
    Value::Object(args)
}

struct Command {
    request: Request,
    binary: Vec<u8>,
    deadline: Instant,
    state: Arc<State>,
    reply: mpsc::SyncSender<Result<Value, String>>,
    connected: Arc<AtomicBool>,
}

// A receipt belongs to its creating connection. Reconnecting, or another
// companion staying connected, must not keep its abandoned resources alive.
struct OwnedResource {
    connected: Arc<AtomicBool>,
    receipt: Value,
}
impl OwnedResource {
    fn belongs_to(&self, connection: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(&self.connected, connection)
    }
}

struct ClientLifetime {
    connected: Arc<AtomicBool>,
    clients: Arc<AtomicUsize>,
}
impl Drop for ClientLifetime {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::Release);
        self.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct Bridge {
    commands: mpsc::Receiver<Command>,
    snapshot: Arc<RwLock<Arc<Value>>>,
    stopped: Arc<AtomicBool>,
    owned_textures: BTreeMap<String, OwnedResource>,
    owned_audio: BTreeMap<String, OwnedResource>,
    manifest: PathBuf,
    token: String,
    session: String,
    port: u16,
    game_root: PathBuf,
    published: Instant,
}

impl Bridge {
    pub(crate) fn from_env(game_root: &std::path::Path) -> Option<Self> {
        if std::env::var("OMSI_NATIVE_BRIDGE").as_deref() != Ok("1") {
            return None;
        }
        match Self::start(game_root) {
            Ok(bridge) => Some(bridge),
            Err(error) => {
                log::error!("Native plugin bridge could not start: {error}");
                None
            }
        }
    }

    fn start(game_root: &std::path::Path) -> io::Result<Self> {
        if let Some(path) = std::env::var_os("OMSI_NATIVE_BRIDGE_MANIFEST") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(io::Error::other("native bridge manifest must be absolute"));
            }
            return Self::start_at_manifest(game_root, &path);
        }
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .ok_or_else(|| io::Error::other("no user profile directory"))?;
        let manifest = PathBuf::from(home).join(".openomsi/native-bridge.json");
        Self::start_at_manifest(game_root, &manifest)
    }

    /// Tests use the exact production listener with an isolated manifest path;
    /// no profile/environment changes or alternate protocol implementation.
    fn start_at_manifest(
        game_root: &std::path::Path,
        manifest: &std::path::Path,
    ) -> io::Result<Self> {
        let parent = manifest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| io::Error::other("bridge manifest needs a parent directory"))?;
        std::fs::create_dir_all(parent)?;
        let manifest = manifest.to_path_buf();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let (send, commands) = mpsc::sync_channel(4);
        let snapshot = Arc::new(RwLock::new(Arc::new(json!(null))));
        let stopped = Arc::new(AtomicBool::new(false));
        let clients = Arc::new(AtomicUsize::new(0));
        let token = random_id();
        let session = random_id();
        let bridge = Self {
            commands,
            snapshot: snapshot.clone(),
            stopped: stopped.clone(),
            manifest,
            owned_textures: BTreeMap::new(),
            owned_audio: BTreeMap::new(),
            token: token.clone(),
            session,
            port,
            game_root: game_root.to_path_buf(),
            published: Instant::now() - Duration::from_secs(1),
        };
        bridge.write_manifest()?;
        std::thread::Builder::new()
            .name("native-bridge-listener".into())
            .spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, address)) if address.ip().is_loopback() => {
                            if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                                continue;
                            }
                            clients.fetch_add(1, Ordering::Relaxed);
                            let (send, snapshot, token, stopped, clients) = (
                                send.clone(),
                                snapshot.clone(),
                                token.clone(),
                                stopped.clone(),
                                clients.clone(),
                            );
                            // The listener is the only incrementer. Each worker owns its decrement.
                            let count = clients.clone();
                            if std::thread::Builder::new()
                                .name("native-bridge-client".into())
                                .spawn(move || {
                                    let lifetime = ClientLifetime {
                                        connected: Arc::new(AtomicBool::new(true)),
                                        clients,
                                    };
                                    serve(
                                        stream,
                                        &token,
                                        &send,
                                        &snapshot,
                                        &stopped,
                                        &lifetime.connected,
                                    );
                                })
                                .is_err()
                            {
                                count.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        Err(_) => break,
                    }
                }
            })?;
        log::info!("Native plugin bridge ready on the local interface, port {port}");
        Ok(bridge)
    }

    #[cfg(test)]
    pub(crate) fn start_for_test(
        game_root: &std::path::Path,
        manifest: &std::path::Path,
    ) -> io::Result<Self> {
        Self::start_at_manifest(game_root, manifest)
    }

    fn write_manifest(&self) -> io::Result<()> {
        let data = serde_json::to_vec(&json!({"protocol":1, "pid":std::process::id(),
            "port":self.port, "token":self.token, "session_id":self.session,
            "game_root":self.game_root.to_string_lossy()}))?;
        // This lives in the user's private application directory; never log its contents.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let temporary = self
            .manifest
            .with_extension(format!("{}.tmp", std::process::id()));
        options.open(&temporary)?.write_all(&data)?;
        let result = std::fs::rename(&temporary, &self.manifest);
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }

    fn update(&mut self, app: &mut App) {
        // Refresh identities before applying queued commands. The API owns identity
        // state, so Lua and external clients always refer to the same live objects.
        let session = crate::plugin_api::session(app);
        if session != self.session {
            self.release_textures(app);
            self.release_audio(app);
            self.session = session;
            // An older game instance must not replace a newer instance's discovery file.
            if self.owns_manifest() {
                if let Err(error) = self.write_manifest() {
                    log::warn!("Native plugin manifest update failed: {error}");
                }
            }
            self.published = Instant::now() - Duration::from_secs(1);
        }
        let mut replies = Vec::new();
        let mut changed = false;
        // One frame does bounded work even when a companion is malfunctioning.
        for _ in 0..4 {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            let result = if command.connected.load(Ordering::Acquire)
                && command.state.begin(command.deadline)
            {
                // Scripts may change state before returning an error.
                changed = true;
                self.apply(app, &command.request, &command.binary, &command.connected)
            } else {
                Err(
                    "command was cancelled or expired before execution; no changes were applied"
                        .into(),
                )
            };
            if result.is_ok()
                && matches!(
                    command.request.op.as_str(),
                    "script_texture" | "texture.upload"
                )
            {
                if let Ok(receipt) = &result {
                    if let Some(resource) = receipt["resource"].as_str() {
                        self.owned_textures.insert(
                            resource.to_string(),
                            OwnedResource {
                                connected: command.connected.clone(),
                                receipt: receipt.clone(),
                            },
                        );
                    }
                }
            }
            if command.request.op == "audio.clip.play" {
                if let Ok(receipt) = &result {
                    if let Some(lease) = receipt["lease"].as_str() {
                        self.owned_audio.insert(lease.to_string(), OwnedResource {
                            connected: command.connected.clone(), receipt: audio_args(receipt),
                        });
                    }
                }
            } else if command.request.op == "audio.clip.release" && result.is_ok() {
                if let Some(lease) = command.request.args.as_ref().and_then(|args| args["lease"].as_str()) {
                    self.owned_audio.remove(lease);
                }
            }
            replies.push((command.reply, result));
        }
        changed |= self.release_disconnected_textures(app);
        changed |= self.release_disconnected_audio(app);
        if changed || self.published.elapsed() >= Duration::from_millis(50) {
            let snapshot = crate::plugin_api::snapshot(app);
            *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(snapshot);
            self.published = Instant::now();
        }
        // Publish the resulting state before acknowledging a write: an immediate
        // subsequent read from that client must not see the preceding snapshot.
        for (reply, result) in replies {
            let _ = reply.try_send(result);
        }
    }

    fn apply(&self, app: &mut App, request: &Request, binary: &[u8], connected: &Arc<AtomicBool>) -> Result<Value, String> {
        if request.session_id != self.session {
            return Err("stale session; read a new snapshot".into());
        }
        let mut args = match &request.args {
            Some(value) => value.clone(),
            None => serde_json::to_value(request).map_err(|e| e.to_string())?,
        };
        let fields = args
            .as_object_mut()
            .ok_or("API arguments must be an object")?;
        fields.remove("token");
        fields.insert("session_id".into(), json!(request.session_id));
        if matches!(request.op.as_str(), "audio.clip.get" | "audio.clip.release") {
            let lease = args["lease"].as_str().ok_or("announcement lease is required")?;
            if !self.owned_audio.get(lease).is_some_and(|owned| owned.belongs_to(connected)) {
                return Err("announcement lease belongs to another connection or has expired".into());
            }
        }
        crate::plugin_api::execute(app, &request.op, args, binary)
    }

    fn release_audio(&mut self, app: &mut App) {
        for (_, owned) in std::mem::take(&mut self.owned_audio) {
            let _ = crate::plugin_api::execute(app, "audio.clip.release", owned.receipt, &[]);
        }
    }

    fn release_disconnected_audio(&mut self, app: &mut App) -> bool {
        let mut changed = false;
        self.owned_audio.retain(|_, owned| {
            if crate::plugin_api::execute(app, "audio.clip.get", owned.receipt.clone(), &[]).is_err() {
                return false;
            }
            if owned.connected.load(Ordering::Acquire) {
                return true;
            }
            changed |= crate::plugin_api::execute(app, "audio.clip.release", owned.receipt.clone(), &[]).is_ok();
            false
        });
        changed
    }

    fn release_textures(&mut self, app: &mut App) {
        // Receipts include a per-upload lease. A local Lua plugin may have replaced
        // the same slot meanwhile; disconnecting this client must not clear its work.
        for (_, owned) in std::mem::take(&mut self.owned_textures) {
            let _ = crate::plugin_api::execute(app, "texture.release", owned.receipt, &[]);
        }
    }

    fn release_disconnected_textures(&mut self, app: &mut App) -> bool {
        let mut changed = false;
        self.owned_textures.retain(|_, owned| {
            if !crate::plugin_api::texture_receipt_is_current(app, &owned.receipt) {
                return false;
            }
            if owned.connected.load(Ordering::Acquire) {
                return true;
            }
            // The exact lease protects a slot subsequently claimed by Lua or a
            // different connection. Rejected stale leases need no further work.
            changed |=
                crate::plugin_api::execute(app, "texture.release", owned.receipt.clone(), &[])
                    .is_ok();
            false
        });
        changed
    }

    #[cfg(test)]
    pub(crate) fn own_texture_for_test(&mut self, connected: Arc<AtomicBool>, receipt: Value) {
        let resource = receipt["resource"]
            .as_str()
            .expect("upload receipt resource")
            .to_owned();
        self.owned_textures
            .insert(resource, OwnedResource { connected, receipt });
    }

    #[cfg(test)]
    pub(crate) fn owned_texture_count_for_test(&self) -> usize {
        self.owned_textures.len()
    }
    fn owns_manifest(&self) -> bool {
        std::fs::read(&self.manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|v| v["token"].as_str() == Some(self.token.as_str()))
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // A second instance may have published its own endpoint in the meantime.
        if self.owns_manifest() {
            let _ = std::fs::remove_file(&self.manifest);
        }
    }
}

pub(crate) fn poll(app: &mut App) {
    if let Some(mut bridge) = app.native_bridge.take() {
        bridge.update(app);
        app.native_bridge = Some(bridge);
    }
}

fn serve(
    mut stream: TcpStream,
    token: &str,
    send: &mpsc::SyncSender<Command>,
    snapshot: &RwLock<Arc<Value>>,
    stopped: &AtomicBool,
    connected: &Arc<AtomicBool>,
) {
    // Windows accepted sockets inherit the nonblocking listener mode. Framed
    // read_exact below needs blocking reads: normal gaps between a header and
    // its payload otherwise look like a failed connection (WSAEWOULDBLOCK).
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(COMMAND_TIMEOUT));
    let _ = stream.set_write_timeout(Some(COMMAND_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let mut period = Instant::now();
    let mut count = 0;
    let mut replay = ReplayGuard::default();
    let mut previous: Option<(u64, Receipt)> = None;
    while !stopped.load(Ordering::Relaxed) {
        let (request, binary) = match read_frame(&mut stream) {
            Ok(frame) => frame,
            Err(error) => {
                log::debug!("bridge connection ended while reading: {:?}", error.kind());
                break;
            }
        };
        if !authenticated(token, &request.token) || request.protocol != 1 {
            break;
        }
        if period.elapsed() >= Duration::from_secs(1) {
            period = Instant::now();
            count = 0;
        }
        count += 1;
        let id = request.request_id;
        let response = if count > 120 {
            json!({"protocol":1,"request_id":id,"ok":false,"error":"request rate exceeded"})
        } else if request.op == "snapshot" {
            let value = snapshot.read().unwrap_or_else(|e| e.into_inner()).clone();
            if value.is_null() {
                json!({"protocol":1,"request_id":id,"ok":false,"error":"first game snapshot is not ready"})
            } else {
                json!({"protocol":1,"request_id":id,"ok":true,"snapshot":&*value})
            }
        } else {
            if previous.as_ref().is_some_and(|(last, _)| *last == id) {
                previous.as_mut().unwrap().1.response(id, COMMAND_TIMEOUT)
            } else if !replay.accept_new(id) {
                json!({"protocol":1,"request_id":id,"ok":false,"status":"duplicate",
                    "error":"request_id was already used or is older than a previous command; it will not be reapplied; read current state and use increasing IDs for new commands"})
            } else {
                let (reply, mut receipt) = Receipt::channel();
                let command = Command {
                    request,
                    binary,
                    reply,
                    state: receipt.state.clone(),
                    deadline: Instant::now() + COMMAND_TIMEOUT,
                    connected: connected.clone(),
                };
                let response = match send.try_send(command) {
                    Ok(()) => receipt.response(id, COMMAND_TIMEOUT),
                    Err(_) => receipt.reject(
                        id,
                        "game command queue is full or unavailable; no changes were applied",
                    ),
                };
                previous = Some((id, receipt));
                response
            }
        };
        if write_response(&mut stream, &response).is_err() || count > 120 {
            break;
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn announcement_cleanup_retains_only_the_exact_voice_identity() {
        let receipt = json!({"id":"9007199254740993","generation":"18446744073709551614",
            "session_id":"session","section":1,"audio_generation":"9876543210","lease":"opaque",
            "index":5,"playing":true,"duration_seconds":3.0,"resource":"diagnostic"});
        let arguments = audio_args(&receipt);
        assert_eq!(arguments["id"], "9007199254740993");
        assert_eq!(arguments["generation"], "18446744073709551614");
        assert_eq!(arguments["lease"], "opaque");
        assert_eq!(arguments.as_object().unwrap().len(), 6);
        assert!(arguments.get("index").is_none());
    }
    #[test]
    fn announcement_lease_is_not_transferred_to_a_reconnected_companion() {
        let original = Arc::new(AtomicBool::new(true));
        let resource = OwnedResource { connected: original.clone(), receipt: json!({}) };
        assert!(resource.belongs_to(&original));
        let other = Arc::new(AtomicBool::new(true));
        assert!(!resource.belongs_to(&other));
        original.store(false, Ordering::Release);
        let reconnected = Arc::new(AtomicBool::new(true));
        assert!(!resource.belongs_to(&reconnected));
    }

    #[test]
    fn fragmented_frames_and_idle_gaps_keep_the_connection_usable() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.set_nodelay(true).unwrap();
        let (server, _) = listener.accept().unwrap();
        // Explicitly reproduce Windows' inherited mode on every test platform.
        server.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            let (send, _commands) = mpsc::sync_channel(4);
            let snapshot = RwLock::new(Arc::new(json!({"ready":true})));
            serve(
                server,
                "fixture",
                &send,
                &snapshot,
                &AtomicBool::new(false),
                &Arc::new(AtomicBool::new(true)),
            );
        });
        for id in 1..=2 {
            let body = serde_json::to_vec(
                &json!({"protocol":1,"request_id":id,"op":"snapshot","token":"fixture"}),
            )
            .unwrap();
            client
                .write_all(&(body.len() as u32).to_le_bytes())
                .unwrap();
            std::thread::sleep(Duration::from_millis(30));
            for chunk in body.chunks(13) {
                client.write_all(chunk).unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
            client.write_all(&0u32.to_le_bytes()).unwrap();
            let mut length = [0; 4];
            client.read_exact(&mut length).unwrap();
            let length = u32::from_le_bytes(length) as usize;
            assert!(length < 1024);
            let mut reply = vec![0; length];
            client.read_exact(&mut reply).unwrap();
            let reply: Value = serde_json::from_slice(&reply).unwrap();
            assert_eq!(reply["request_id"], id);
            assert_eq!(reply["snapshot"]["ready"], true);
            let mut binary = [0; 4];
            client.read_exact(&mut binary).unwrap();
            assert_eq!(binary, [0; 4]);
            std::thread::sleep(Duration::from_millis(30));
        }
        drop(client);
        worker.join().unwrap();
    }
}
