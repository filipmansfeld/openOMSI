//! The game's log as a record of the whole session (`game.log`, which the launcher keeps per
//! session): the machine and the settings at the start, and while playing what was said on
//! the screen, the views, the pause, and a status line every minute - what a bug report
//! needs to be understood without asking.

use crate::App;

#[derive(Default)]
pub(crate) struct LogState {
    last_msg: Option<String>,
    last_view: String,
    last_paused: bool,
    t: f32,
    frames: u32,
    worst_dt: f32,
}

/// The machine, the program and its settings, once at the start.
pub(crate) fn log_system(settings: &crate::settings::Settings) {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let ram = crate::memory::physical_memory().map(|b| b / 1_000_000).unwrap_or(0);
    log::info!("system: {} {} ({}), {cpus} threads, {ram} MB memory", std::env::consts::OS, std::env::consts::ARCH, os_version());
    log::info!("command line: {}", std::env::args().collect::<Vec<_>>().join(" "));
    if let Ok(d) = std::env::current_dir() {
        log::info!("working folder: {}", d.display());
    }
    let env = logged_environment(std::env::vars());
    if !env.is_empty() {
        log::info!("environment: {}", env.join(" "));
    }
    log::info!("all settings: {settings:?}");
}

/// Only diagnostic configuration belongs in logs; handoff and credential values stay private.
fn logged_environment(vars: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    vars.into_iter()
        .filter(|(key, _)| {
            let key = key.to_ascii_uppercase();
            if key == "RUST_LOG" || key == "WGPU_BACKEND" {
                return true;
            }
            key.starts_with("OMSI_")
                && key != "OMSI_UI_CAP"
                && key != "OMSI_UI_PIPE"
                && !["TOKEN", "SECRET", "PASSWORD", "CAPABILITY"].iter().any(|secret| key.contains(*secret))
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(o) = std::process::Command::new("sw_vers").arg("-productVersion").output() {
            return format!("macOS {}", String::from_utf8_lossy(&o.stdout).trim());
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
            if let Some(l) = s.lines().find(|l| l.starts_with("PRETTY_NAME=")) {
                return l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string();
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(o) = std::process::Command::new("cmd").args(["/C", "ver"]).output() {
            return String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
    }
    "version unknown".to_string()
}

impl App {
    /// Once a frame: what changed on the screen, and every minute where things stand.
    pub(crate) fn log_frame(&mut self, dt: f32) {
        let msg = self.service_msg.as_ref().map(|m| m.0.clone());
        if msg.is_some() && msg != self.log_state.last_msg {
            log::info!("on screen: {}", msg.as_deref().unwrap_or_default());
        }
        self.log_state.last_msg = msg;
        if self.view != self.log_state.last_view {
            log::info!("view: {}", self.view);
            self.log_state.last_view = self.view.clone();
        }
        if self.paused != self.log_state.last_paused {
            log::info!("{}", if self.paused { "paused" } else { "resumed" });
            self.log_state.last_paused = self.paused;
        }
        let s = &mut self.log_state;
        s.t += dt;
        s.frames += 1;
        s.worst_dt = s.worst_dt.max(dt);
        if s.t < 60.0 {
            return;
        }
        let (fps, worst) = (s.frames as f32 / s.t, 1.0 / s.worst_dt.max(1e-3));
        s.t = 0.0;
        s.frames = 0;
        s.worst_dt = 0.0;
        let bus = self.player.as_ref().map(|p| {
            let v = &p.vehicle;
            format!("bus at ({:.1}, {:.1}, {:.1}) heading {:.0}, {:.0} km/h", v.position.x, v.position.y, v.position.z, v.heading, v.physics.velocity_kmh())
        });
        let cam = self.camera.as_ref().map(|c| format!("camera at ({:.0}, {:.0}, {:.0})", c.position.x, c.position.y, c.position.z));
        let traffic = self.traffic.as_ref().map(|t| t.cars.len()).unwrap_or(0);
        let time = self.clock.time;
        let gpu = match (self.renderer.as_ref(), self.scene.as_ref()) {
            (Some(r), Some(sc)) => format!(", GPU memory: textures {:.0} MB, meshes {:.0} MB", r.texture_bytes(sc) as f64 / 1e6, r.mesh_bytes(sc) as f64 / 1e6),
            _ => String::new(),
        };
        log::info!(
            "status: {fps:.0} fps (worst frame {worst:.0} fps), {}, view {}, game time {:02}:{:02}, {traffic} AI vehicles{}{gpu}",
            bus.or(cam).unwrap_or_default(),
            self.view,
            (time / 3600.0) as i64 % 24,
            (time / 60.0) as i64 % 60,
            if self.paused { ", paused" } else { "" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::logged_environment;

    #[test]
    fn environment_log_omits_private_handoff_and_credentials() {
        let vars = [
            ("OMSI_UI_CAP", "assertneverlog-ui-cap"),
            ("oMsI_uI_pIpE", "assertneverlog-ui-pipe"),
            ("OMSI_access_ToKeN", "assertneverlog-token"),
            ("omsi_client_secret", "assertneverlog-secret"),
            ("OmSi_Auth_Password", "assertneverlog-password"),
            ("OMSI_bridge_cApAbIlItY", "assertneverlog-capability"),
            ("UNRELATED_SETTING", "assertneverlog-unrelated"),
            ("OMSI_CONTENT", "C:/OMSI"),
            ("RUST_LOG", "info"),
            ("WGPU_BACKEND", "dx12"),
        ];
        let logged = logged_environment(vars.into_iter().map(|(key, value)| (key.to_owned(), value.to_owned())));
        assert!(!logged.iter().any(|entry| entry.contains("assertneverlog")));
        assert_eq!(logged, vec![
            "OMSI_CONTENT=C:/OMSI".to_owned(),
            "RUST_LOG=info".to_owned(),
            "WGPU_BACKEND=dx12".to_owned(),
        ]);
    }
}
