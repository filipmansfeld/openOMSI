//! Full portable Windows client updates. The engine and private helpers are one
//! verified package; the ordinary engine-only archive installer is never used.
use super::{version_parts, Release};
use anyhow::{anyhow, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 64 * 1024;
const INSTALLER: &str = "Update-PSTClient.ps1";
const STARTER: &str = "Start-NativeClient7.ps1";

#[derive(Clone, Debug)]
pub(super) struct Client {
    root: PathBuf,
    anchor: PathBuf,
    exe: PathBuf,
}

#[derive(Deserialize)]
struct Manifest {
    schema: u32,
    version: String,
    verified_build: bool,
    source_commit: String,
    source_tree: String,
    engine: String,
    files: Vec<File>,
}
#[derive(Deserialize)]
struct File {
    path: String,
    bytes: u64,
    sha256: String,
}

fn safe_member(root: &Path, authored: &str) -> anyhow::Result<PathBuf> {
    ensure!(!authored.is_empty() && authored.len() <= 1024 && !authored.contains(':'), "invalid portable member path");
    let normalized = authored.replace('\\', "/");
    let relative = Path::new(&normalized);
    ensure!(relative.components().all(|c| matches!(c, Component::Normal(_))), "portable member escaped its package");
    let path = root.join(relative).canonicalize()?;
    ensure!(path.starts_with(root) && path.is_file(), "portable member escaped its package");
    Ok(path)
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut input = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 256 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_package(root: &Path, version: &str, running: Option<&Path>) -> anyhow::Result<PathBuf> {
    ensure!(version_parts(version).is_some(), "invalid portable client version");
    let root = root.canonicalize()?;
    let manifest_path = root.join("release-manifest.json");
    let metadata = std::fs::metadata(&manifest_path)?;
    ensure!(metadata.len() <= MANIFEST_LIMIT && metadata.is_file(), "invalid portable manifest size");
    let text = std::fs::read(&manifest_path)?;
    ensure!(text.len() as u64 <= MANIFEST_LIMIT, "invalid portable manifest size");
    let manifest: Manifest = serde_json::from_slice(&text)?;
    ensure!(manifest.schema == 1 && manifest.verified_build && manifest.version == version,
        "portable manifest does not match this verified client");
    for identity in [&manifest.source_commit, &manifest.source_tree] {
        ensure!(identity.len() == 40 && identity.bytes().all(|c| c.is_ascii_hexdigit()), "invalid portable source identity");
    }
    ensure!(!manifest.files.is_empty() && manifest.files.len() <= 8192, "invalid portable file inventory");
    let expected_engine = format!("bin/Tangenta-client-{version}/openomsi.exe");
    ensure!(manifest.engine == expected_engine, "portable engine layout does not match its version");
    let exe = safe_member(&root, &manifest.engine)?;
    if let Some(running) = running { ensure!(exe == running.canonicalize()?, "portable root does not contain this running engine"); }
    // These files are needed before an update can be offered or handed to the
    // installer. The installer verifies the complete archive and its manifest.
    for required in [manifest.engine.as_str(), INSTALLER, STARTER, "Start-PST.ps1", "NativeClient7.Common.ps1", "NativeBackend/Tangenta.NativeBackend.exe",
        "NativeBackend/Tangenta.NativeBackend.dll", "NativeBackend/Tangentalauncher.dll"] {
        let matches: Vec<_> = manifest.files.iter().filter(|f| f.path.eq_ignore_ascii_case(required)).collect();
        ensure!(matches.len() == 1, "portable manifest omitted or duplicated a required member");
        let record = matches[0];
        ensure!(record.bytes > 0 && record.sha256.len() == 64 && record.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid portable member identity");
        let path = safe_member(&root, required)?;
        ensure!(std::fs::metadata(&path)?.len() == record.bytes && hash_file(&path)?.eq_ignore_ascii_case(&record.sha256),
            "portable client member failed verification");
    }
    Ok(exe)
}

impl Client {
    pub(super) fn directory(&self) -> &Path { &self.root }

    pub(super) fn download_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        ensure!(Path::new(name).components().count() == 1 && Path::new(name).components().all(|c| matches!(c, Component::Normal(_))) && !name.contains(['/', '\\', ':']), "invalid portable archive name");
        let mut directory = self.anchor.clone();
        for part in [".pst-updates", "downloads"] {
            let next = directory.join(part);
            if !next.exists() { std::fs::create_dir(&next)?; }
            directory = next.canonicalize()?;
            ensure!(directory.starts_with(&self.anchor) && directory.is_dir(), "portable download folder escaped its anchor");
        }
        Ok(directory.join(name))
    }
}

pub(super) fn asset(version: &str) -> Option<String> {
    Some(format!("PST-{}.zip", version_parts(version)?.revision))
}

pub(super) fn current() -> anyhow::Result<Option<Client>> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        static CLIENT: std::sync::OnceLock<Result<Option<Client>, String>> = std::sync::OnceLock::new();
        let result = CLIENT.get_or_init(|| (|| -> anyhow::Result<Option<Client>> {
            let Some(root) = std::env::var_os("TANGENTA_PORTABLE_RUNTIME_ROOT") else { return Ok(None); };
            let root = PathBuf::from(root).canonicalize()?;
            let exe = std::env::current_exe()?.canonicalize()?;
            validate_package(&root, super::current_version(), Some(&exe))?;
            let anchor = std::env::var_os("TANGENTA_UPDATE_ANCHOR_ROOT").map(PathBuf::from).unwrap_or_else(|| root.clone()).canonicalize()?;
            ensure!(anchor.is_dir(), "invalid portable update anchor");
            Ok(Some(Client { root, anchor, exe }))
        })().map_err(|e| format!("{e:#}")));
        result.clone().map_err(|e| anyhow!(e))
    }
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    { Ok(None) }
}

fn installer_arguments(client: &Client, release: &Release, archive: Option<&Path>) -> Vec<std::ffi::OsString> {
    let mut args = vec!["-Mode".into(), (if archive.is_some() { "Install" } else { "Preflight" }).into(), "-PackageRoot".into(), client.root.as_os_str().to_owned(),
        "-AnchorRoot".into(), client.anchor.as_os_str().to_owned(), "-TargetVersion".into(), release.version.clone().into(),
        "-ExpectedArchiveSha256".into(), release.sha256.clone().unwrap_or_default().into(), "-ExpectedArchiveBytes".into(), release.size.to_string().into()];
    if let Some(archive) = archive { args.push("-ArchivePath".into()); args.push(archive.as_os_str().to_owned()); }
    shared_roots(&mut args);
    args
}

fn shared_roots(args: &mut Vec<std::ffi::OsString>) {
    for (name, parameter) in [("HOME", "-ProfileRoot"), ("OMSI_CONTENT", "-ContentRoot"), ("OMSI_ROOT", "-GameRoot")] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) { args.push(parameter.into()); args.push(value); }
    }
}

fn command(client: &Client) -> anyhow::Result<std::process::Command> {
    // Recheck the old installer before either Install or Relaunch. A cached
    // context must never authorize a script that changed after the update check.
    validate_package(&client.root, super::current_version(), Some(&client.exe))?;
    let system = std::env::var_os("SystemRoot").map(PathBuf::from).ok_or_else(|| anyhow!("Windows system folder is unknown"))?;
    ensure!(system.is_absolute(), "invalid Windows system folder");
    let mut command = std::process::Command::new(system.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(safe_member(&client.root, INSTALLER)?).current_dir(&client.root)
        .env_remove("OMSI_LAUNCHER_INPUT").stdin(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    Ok(command)
}

#[cfg(target_os = "windows")]
struct InstallerJob(windows::Win32::Foundation::HANDLE);
#[cfg(target_os = "windows")]
impl InstallerJob {
    fn attach(child: &std::process::Child) -> anyhow::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::*;
        // The verified Install/Preflight script never starts the game. Its owned
        // validation children must not hold inherited pipes after an abort.
        let job = Self(unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null())? });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(job.0, JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void, std::mem::size_of_val(&limits) as u32)?;
            AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle()))?;
        }
        Ok(job)
    }
}
#[cfg(target_os = "windows")]
impl Drop for InstallerJob {
    fn drop(&mut self) { unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.0); } }
}

fn safe_installer_error(output: &[u8]) -> Option<&'static str> {
    let reply = serde_json::from_slice::<serde_json::Value>(output).ok()?;
    (reply["status"].as_str() == Some("failed") && reply["code"].as_str() == Some("disk_reserve_5gib_required"))
        .then_some("Uvolněte místo pro nového klienta a ponechte alespoň 5 GiB volných.")
}

fn bounded_output(mut command: std::process::Command) -> anyhow::Result<Vec<u8>> {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
    let mut child = command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
    #[cfg(target_os = "windows")]
    let job = match InstallerJob::attach(&child) {
        Ok(job) => job,
        Err(error) => { let _ = child.kill(); let _ = child.wait(); return Err(error); }
    };
    let overflow = Arc::new(AtomicBool::new(false));
    fn reader(stream: impl Read + Send + 'static, overflow: Arc<AtomicBool>) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let result = stream.take(OUTPUT_LIMIT + 1).read_to_end(&mut data).map(|_| data);
            if result.as_ref().is_ok_and(|data| data.len() as u64 > OUTPUT_LIMIT) { overflow.store(true, Ordering::Release); }
            let _ = sender.send(result);
        });
        receiver
    }
    let out = reader(child.stdout.take().ok_or_else(|| anyhow!("installer output is missing"))?, overflow.clone());
    let err = reader(child.stderr.take().ok_or_else(|| anyhow!("installer error output is missing"))?, overflow.clone());
    let started = std::time::Instant::now();
    let completion = loop {
        if overflow.load(Ordering::Acquire) { break Err(anyhow!("portable installer output exceeds its limit")); }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => (),
            Err(error) => break Err(error.into()),
        }
        if started.elapsed() > std::time::Duration::from_secs(900) { break Err(anyhow!("portable installer did not finish within its limit")); }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // Closing this owned job terminates only Install/Preflight and its children.
    // Relaunch and the user's running game never belong to it.
    #[cfg(target_os = "windows")]
    drop(job);
    if completion.is_err() { let _ = child.kill(); let _ = child.wait(); }
    let status = completion?;
    let stdout = out.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| anyhow!("installer output did not close"))??;
    let stderr = err.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| anyhow!("installer error output did not close"))??;
    ensure!(stdout.len() as u64 <= OUTPUT_LIMIT && stderr.len() as u64 <= OUTPUT_LIMIT, "portable installer output exceeds its limit");
    if !status.success() {
        if let Some(message) = safe_installer_error(&stdout) { anyhow::bail!(message); }
        anyhow::bail!("portable installation failed; the previous client was retained");
    }
    Ok(stdout)
}

fn ready(client: &Client, release: &Release, output: &[u8]) -> anyhow::Result<()> {
    ensure!(output.len() as u64 <= OUTPUT_LIMIT, "portable installer output exceeds its limit");
    let reply: serde_json::Value = serde_json::from_slice(output)?;
    ensure!(reply["status"].as_str() == Some("ready") && reply["version"].as_str() == Some(release.version.as_str()), "installer did not confirm the requested client");
    let root = PathBuf::from(reply["package_root"].as_str().ok_or_else(|| anyhow!("installer did not return a package root"))?).canonicalize()?;
    let anchor = PathBuf::from(reply["anchor_root"].as_str().ok_or_else(|| anyhow!("installer did not return an update anchor"))?).canonicalize()?;
    let expected = client.anchor.join(".pst-updates/releases").join(&release.version).canonicalize()?;
    ensure!(anchor == client.anchor && root == expected && root != client.root,
        "installer returned an unrelated client installation");
    validate_package(&root, &release.version, None)?;
    for (name, field) in [("HOME", "profile_root"), ("OMSI_CONTENT", "content_root"), ("OMSI_ROOT", "game_root")] {
        if let Some(expected) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            let actual = reply[field].as_str().ok_or_else(|| anyhow!("installer omitted a shared root"))?;
            ensure!(PathBuf::from(actual).canonicalize()? == PathBuf::from(expected).canonicalize()?, "installer changed a shared root");
        }
    }
    Ok(())
}

fn check_preflight(release: &Release, output: &[u8]) -> anyhow::Result<()> {
    ensure!(output.len() as u64 <= OUTPUT_LIMIT, "portable installer output exceeds its limit");
    let reply: serde_json::Value = serde_json::from_slice(output)?;
    ensure!(reply["status"].as_str() == Some("preflight") && reply["version"].as_str() == Some(release.version.as_str())
        && reply["required_free_bytes"].as_u64().is_some_and(|bytes| bytes > 0), "installer did not approve download preflight");
    Ok(())
}

pub(super) fn preflight(client: &Client, release: &Release) -> anyhow::Result<()> {
    let mut command = command(client)?;
    command.args(installer_arguments(client, release, None));
    check_preflight(release, &bounded_output(command)?)
}

pub(super) fn install(client: &Client, release: &Release, archive: &Path) -> anyhow::Result<()> {
    ensure!(asset(&release.version).as_deref() == Some(release.asset_name.as_str()), "integrated client requires its complete PST archive");
    let mut command = command(client)?;
    command.args(installer_arguments(client, release, Some(archive)));
    let output = bounded_output(command)?;
    ready(client, release, &output)
}

pub(super) fn relaunch(client: &Client) -> anyhow::Result<()> {
    let mut command = command(client)?;
    let mut args = vec!["-Mode".into(), "Relaunch".into(), "-PackageRoot".into(), client.anchor.as_os_str().to_owned(),
        "-WaitForPid".into(), std::process::id().to_string().into(), "-ExpectedEnginePath".into(), client.exe.as_os_str().to_owned()];
    shared_roots(&mut args);
    command.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path, version: &str) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let engine = format!("bin/Tangenta-client-{version}/openomsi.exe");
        let members = [&engine, INSTALLER, STARTER, "Start-PST.ps1", "NativeClient7.Common.ps1", "NativeBackend/Tangenta.NativeBackend.exe",
            "NativeBackend/Tangenta.NativeBackend.dll", "NativeBackend/Tangentalauncher.dll"];
        let files: Vec<_> = members.iter().map(|path| {
            let file = root.join(path); std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, format!("verified fixture {path} {version}")).unwrap();
            serde_json::json!({ "path":path, "bytes":std::fs::metadata(&file).unwrap().len(), "sha256":hash_file(&file).unwrap() })
        }).collect();
        std::fs::write(root.join("release-manifest.json"), serde_json::to_vec(&serde_json::json!({
            "schema":1,"version":version,"verified_build":true,"source_commit":"ab".repeat(20),"source_tree":"cd".repeat(20),
            "engine":engine,"files":files
        })).unwrap()).unwrap();
        root.join(engine)
    }

    #[test]
    fn complete_pst_manifest_selection_and_installer_boundary() {
        let temp = std::env::temp_dir().join(format!("pst-updater-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&temp).unwrap();
        let version = "0.1.1324-tangenta.10";
        let next = "0.1.1324-tangenta.11";
        let old_root = temp.join("current"); let new_root = temp.join(".pst-updates/releases").join(next);
        let engine = fixture(&old_root, version);
        let next_engine = fixture(&new_root, next);
        let client = Client { root:old_root.canonicalize().unwrap(),anchor:temp.canonicalize().unwrap(),exe:engine.canonicalize().unwrap() };
        let release = Release { version:next.into(),page:format!("{}/releases/tag/v{next}",super::super::REPO_URL), notes:String::new(),
            asset_name:asset(next).unwrap(),asset_url:format!("{}/releases/download/v{next}/PST-11.zip",super::super::REPO_URL),size:42,sha256:Some("ab".repeat(32)) };
        assert_eq!(asset(next), Some("PST-11.zip".into()));
        assert!(validate_package(&old_root, version, Some(&engine)).is_ok());
        assert!(validate_package(&old_root, next, Some(&engine)).is_err());
        assert!(validate_package(&old_root, version, Some(&next_engine)).is_err());
        let args = installer_arguments(&client, &release, Some(&temp.join("PST-11.zip")));
        let args: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
        for expected in ["Install", "-ExpectedArchiveSha256", "-ExpectedArchiveBytes", "-ArchivePath", "-AnchorRoot"] {
            assert!(args.iter().any(|arg| arg == expected));
        }
        let preflight_args = installer_arguments(&client, &release, None);
        assert!(preflight_args.iter().any(|a| a == "Preflight") && !preflight_args.iter().any(|a| a == "-ArchivePath"));
        let preflight = serde_json::to_vec(&serde_json::json!({"status":"preflight","version":next,"required_free_bytes":6_000_000_000u64})).unwrap();
        assert!(check_preflight(&release, &preflight).is_ok());
        assert!(safe_installer_error(br#"{"status":"failed","code":"disk_reserve_5gib_required"}"#).is_some());
        assert!(safe_installer_error(br#"{"status":"failed","code":"untrusted arbitrary detail"}"#).is_none());
        assert!(check_preflight(&release, br#"{"status":"ready","version":"wrong"}"#).is_err());
        assert!(super::super::ensure_download_chunk(40, 2, 42).is_ok());
        assert!(super::super::ensure_download_chunk(40, 3, 42).is_err());
        assert!(client.download_path("../outside.zip").is_err());
        assert!(client.download_path("PST-11.zip").unwrap().starts_with(temp.join(".pst-updates/downloads")));
        let mut reply = serde_json::json!({"status":"ready","version":next,"package_root":new_root,"anchor_root":temp});
        for (name, field) in [("HOME", "profile_root"), ("OMSI_CONTENT", "content_root"), ("OMSI_ROOT", "game_root")] {
            if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) { reply[field] = serde_json::json!(PathBuf::from(value)); }
        }
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(ready(&client, &release, &reply).is_ok());
        std::fs::remove_file(new_root.join("NativeBackend/Tangentalauncher.dll")).unwrap();
        assert!(ready(&client, &release, &reply).is_err());
        let outside = serde_json::to_vec(&serde_json::json!({"status":"ready","version":next,"package_root":old_root,"anchor_root":temp})).unwrap();
        assert!(ready(&client, &release, &outside).is_err());
        let engine_only = serde_json::json!({"tag_name":format!("v{next}"),"html_url":release.page,"assets":[{
            "name":super::super::asset_name(next).unwrap(),"browser_download_url":release.asset_url,"size":42,"digest":format!("sha256:{}","ab".repeat(32))}]});
        assert!(super::super::parse_release_for_mode(&engine_only, version, true).unwrap().is_none());
        let complete = serde_json::json!({"tag_name":format!("v{next}"),"html_url":release.page,"assets":[{
            "name":"PST-11.zip","browser_download_url":release.asset_url,"size":42,"digest":format!("sha256:{}","ab".repeat(32))}]});
        assert_eq!(super::super::parse_release_for_mode(&complete, version, true).unwrap().unwrap().asset_name,"PST-11.zip");
        let mut oversized = complete.clone(); oversized["assets"][0]["size"] = serde_json::json!(2u64 * 1024 * 1024 * 1024 + 1);
        assert!(super::super::parse_release_for_mode(&oversized, version, true).is_err());
        let _ = std::fs::remove_dir_all(temp);
    }
}
