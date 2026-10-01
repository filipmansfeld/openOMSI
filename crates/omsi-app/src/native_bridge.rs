//! Opt-in loopback access to the current vehicle's script state.
//! Socket workers read immutable snapshots and queue bounded commands; only the
//! game frame executes scripts or changes the player's vehicle.

use crate::App;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};

mod command;
pub(crate) mod protocol;
mod vehicle;
use command::{Receipt, ReplayGuard, State};
use protocol::{authenticated, read_frame, write_response, Request};
use vehicle::{random_id, Context};
const MAX_CLIENTS: usize = 4;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

struct Command {
    request: Request,
    deadline: Instant,
    state: Arc<State>,
    reply: mpsc::SyncSender<Result<Value, String>>,
    connected: Arc<AtomicBool>,
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
    context: Context,
    manifest: PathBuf,
    token: String,
    port: u16,
    game_root: PathBuf,
    published: Instant,
    sequence: u64,
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
        let context = Context::new();
        let bridge = Self {
            commands,
            snapshot: snapshot.clone(),
            stopped: stopped.clone(),
            manifest,
            context,
            token: token.clone(),
            port,
            game_root: game_root.to_path_buf(),
            published: Instant::now() - Duration::from_secs(1),
            sequence: 0,
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
            "port":self.port, "token":self.token, "session_id":self.context.session(),
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
        if self.context.refresh(app) {
            // Another game instance may now own discovery. A map change in this
            // older instance must not publish its endpoint over the newer one.
            if self.owns_manifest() {
                if let Err(error) = self.write_manifest() {
                    log::warn!("Native plugin manifest update failed: {error}");
                }
            }
            self.published = Instant::now() - Duration::from_secs(1);
        }
        let mut replies = Vec::new();
        let mut executed = false;
        for _ in 0..4 {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            let result = if command.connected.load(Ordering::Acquire)
                && command.state.begin(command.deadline)
            {
                executed = true;
                self.context.apply(app, &command.request)
            } else {
                Err(
                    "command was cancelled or expired before execution; no changes were applied"
                        .into(),
                )
            };
            replies.push((command.reply, result));
        }
        // A script can modify variables before reporting an execution error.
        // Publish those changes too, before acknowledging any executed command.
        if executed || self.published.elapsed() >= Duration::from_millis(50) {
            self.sequence = self.sequence.wrapping_add(1);
            let mut value = self.context.snapshot(app);
            value["sequence"] = json!(self.sequence);
            *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(value);
            self.published = Instant::now();
        }
        for (reply, result) in replies {
            let _ = reply.try_send(result);
        }
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
        let request = match read_frame(&mut stream) {
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
        } else if !matches!(
            request.op.as_str(),
            "snapshot"
                | "set_variables"
                | "vehicle.set_variables"
                | "vehicle.trigger"
                | "invalidate_texture"
        ) {
            json!({"protocol":1,"request_id":id,"ok":false,"status":"rejected",
                "error":"unsupported operation; no changes were applied"})
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

    fn send(stream: &mut TcpStream, request: Value) {
        let bytes = serde_json::to_vec(&request).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
    }

    fn reply(stream: &mut TcpStream) -> Value {
        let mut size = [0; 4];
        stream.read_exact(&mut size).unwrap();
        let size = u32::from_le_bytes(size) as usize;
        assert!(size < 1024);
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).unwrap();
        let mut binary = [0; 4];
        stream.read_exact(&mut binary).unwrap();
        assert_eq!(binary, [0; 4]);
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn shutdown_preserves_a_manifest_replaced_by_another_instance() {
        let directory = std::env::temp_dir().join(format!("openomsi-bridge-{}", random_id()));
        let manifest = directory.join("native-bridge.json");
        let bridge = Bridge::start_for_test(std::path::Path::new("fixture"), &manifest).unwrap();
        let discovery: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(discovery["protocol"], 1);
        assert_eq!(discovery["port"], bridge.port);
        assert_eq!(discovery["session_id"], bridge.context.session());
        let replacement = b"{\"token\":\"another-instance\"}";
        std::fs::write(&manifest, replacement).unwrap();
        assert!(!bridge.owns_manifest());
        drop(bridge);
        assert_eq!(std::fs::read(&manifest).unwrap(), replacement);
        std::fs::remove_file(&manifest).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn mutations_replay_the_receipt_and_authentication_gates_the_queue() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let (send_queue, commands) = mpsc::sync_channel(4);
        let worker = std::thread::spawn(move || {
            serve(
                server,
                "fixture",
                &send_queue,
                &RwLock::new(Arc::new(json!(null))),
                &AtomicBool::new(false),
                &Arc::new(AtomicBool::new(true)),
            );
        });
        let mut request = json!({"protocol":1,"request_id":7,"token":"fixture",
            "op":"vehicle.trigger","session_id":"session",
            "args":{"id":"1","generation":"2","name":"increment"}});
        send(&mut client, request.clone());
        let command = commands.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(command.state.begin(command.deadline));
        command.reply.send(Ok(json!(null))).unwrap();
        let receipt = reply(&mut client);
        assert_eq!(receipt["ok"], true);
        assert!(receipt.get("result").unwrap().is_null());
        send(&mut client, request.clone());
        assert_eq!(reply(&mut client), receipt);
        assert!(
            commands.try_recv().is_err(),
            "a retried trigger must not execute twice"
        );
        request["request_id"] = json!(6);
        send(&mut client, request.clone());
        assert_eq!(reply(&mut client)["status"], "duplicate");
        request["request_id"] = json!(8);
        request["op"] = json!("clock.set");
        send(&mut client, request.clone());
        assert_eq!(reply(&mut client)["status"], "rejected");
        assert!(commands.try_recv().is_err());
        request["op"] = json!("vehicle.trigger");
        request["token"] = json!("wrong");
        send(&mut client, request);
        assert_eq!(client.read(&mut [0u8; 1]).unwrap(), 0);
        worker.join().unwrap();
        assert!(commands.try_recv().is_err());
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
