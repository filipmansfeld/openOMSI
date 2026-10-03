//! The protected server's explicit Join action. Tickets travel only through a bounded
//! anonymous pipe and RAM; helper output and credentials are never logged.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::{Mutex, MutexGuard, TryLockError};

const LIMIT: usize = 8192;
const DEADLINE: Duration = Duration::from_secs(5 * 60);
const FAILED: &str = "Discord sign-in could not be completed. Try Join again.";
const CANCELLED: &str = "Discord sign-in cancelled. You have not joined the server.";

#[cfg(windows)]
static AUTHORIZATION: Mutex<()> = Mutex::new(());

fn authorization_slot<'a>(lock: &'a Mutex<()>, cancel: &AtomicBool, started: Instant, deadline: Duration) -> Result<MutexGuard<'a, ()>, String> {
    loop {
        if cancel.load(Ordering::Acquire) { return Err(CANCELLED.into()); }
        if started.elapsed() >= deadline { return Err("Discord sign-in timed out. Press Join to try again.".into()); }
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
            Err(TryLockError::Poisoned(_)) => return Err(FAILED.into()),
        }
    }
}

// Deliberately neither Debug nor Serialize: a ticket must not become a status or file.
pub struct Ticket {
    pub token: String,
    pub expires_at: SystemTime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    game_token: String,
    expires_at: String,
    discord_user_id: String,
    display_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Failure {
    error_code: String,
    message: String,
}

fn error_message(code: &str) -> &'static str {
    match code {
        "cancelled" => CANCELLED,
        "denied" => "This Discord account cannot join the Tangenta server.",
        "expired" => "Discord sign-in expired. Press Join to sign in again.",
        "unavailable" => "The sign-in service is unavailable. Try Join again later.",
        "configuration" => "The Tangenta sign-in helper is missing or needs an update.",
        _ => FAILED,
    }
}

fn parse_reply(bytes: &[u8], exit: Option<i32>, now: SystemTime) -> Result<Ticket, String> {
    if bytes.len() > LIMIT { return Err(FAILED.into()); }
    if exit != Some(0) {
        let failure: Failure = serde_json::from_slice(bytes).map_err(|_| FAILED.to_string())?;
        if !matches!(exit, Some(1 | 2)) || !matches!(failure.error_code.as_str(), "configuration" | "expired" | "protocol" | "denied" | "unavailable" | "cancelled")
            || (exit == Some(2) && failure.error_code != "cancelled") || failure.message.is_empty()
            || failure.message.encode_utf16().count() > 512 || failure.message.chars().any(char::is_control) {
            return Err(FAILED.into());
        }
        return Err(error_message(&failure.error_code).into());
    }
    let reply: Reply = serde_json::from_slice(bytes).map_err(|_| FAILED.to_string())?;
    if reply.game_token.len() != 43 || !reply.game_token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        || !(17..=20).contains(&reply.discord_user_id.len()) || !reply.discord_user_id.bytes().all(|b| b.is_ascii_digit())
        || !reply.discord_user_id.parse::<u64>().is_ok_and(|id| id > 0)
        || reply.display_name.is_empty() || reply.display_name.encode_utf16().count() > 128 || reply.display_name.chars().any(char::is_control) {
        return Err(FAILED.into());
    }
    let expires_at = parse_utc(&reply.expires_at).ok_or_else(|| FAILED.to_string())?;
    let remaining = expires_at.duration_since(now).map_err(|_| error_message("expired").to_string())?;
    // This only tolerates local clock skew while reading an 8-hour server ticket. Its
    // original expiry is retained, and the server still enforces the 8-hour lifetime.
    if remaining.is_zero() || remaining > Duration::from_secs(8 * 60 * 60 + 120) { return Err(FAILED.into()); }
    Ok(Ticket { token: reply.game_token, expires_at })
}

/// The helper emits .NET round-trip UTC dates, with up to seven fractional digits.
/// Accept UTC only and validate every calendar component, without a new dependency.
fn parse_utc(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z').or_else(|| text.strip_suffix("+00:00"))?;
    if text.len() < 19 || text.len() > 27 || !text.is_ascii() { return None; }
    let b = text.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' { return None; }
    let number = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<u64>().ok()).flatten();
    let (year, month, day) = (number(&text[..4])?, number(&text[5..7])?, number(&text[8..10])?);
    let (hour, minute, second) = (number(&text[11..13])?, number(&text[14..16])?, number(&text[17..19])?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 { return None; }
    let leap = |y: u64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let lengths = [31, if leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day == 0 || day > lengths[(month - 1) as usize] { return None; }
    let leap_days = |y: u64| y / 4 - y / 100 + y / 400;
    let days = (year - 1970) * 365 + leap_days(year - 1) - leap_days(1969)
        + lengths[..(month - 1) as usize].iter().sum::<u64>() + day - 1;
    let nanos = if text.len() == 19 { 0 } else {
        if b[19] != b'.' { return None; }
        let fraction = &text[20..];
        if fraction.is_empty() || fraction.len() > 7 { return None; }
        number(fraction)? * 10u64.pow(9 - fraction.len() as u32)
    };
    UNIX_EPOCH.checked_add(Duration::new(days * 86400 + hour * 3600 + minute * 60 + second, nanos as u32))
}

fn helper_path(exe: &Path) -> Option<PathBuf> {
    let runtime = exe.parent()?;
    if !runtime.file_name()?.to_str()?.starts_with("Tangenta-client-") { return None; }
    let bin = runtime.parent()?;
    if bin.file_name()? != "bin" { return None; }
    let root = bin.parent()?.canonicalize().ok()?;
    let helper = root.join("auth").join("Tangenta.DiscordLogin.exe").canonicalize().ok()?;
    // A junction must not redirect the trusted helper outside the installed package.
    (helper.starts_with(&root) && helper.is_file()).then_some(helper)
}

#[cfg(windows)]
pub fn authorize(cancel: Arc<AtomicBool>) -> Result<Ticket, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let started = Instant::now();
    // A cancelled predecessor must kill/reap its helper (and release localhost OAuth
    // callback) before the next attempt starts. Waiting is confined to this worker.
    let _slot = authorization_slot(&AUTHORIZATION, &cancel, started, DEADLINE)?;
    let helper = std::env::current_exe().ok().and_then(|exe| helper_path(&exe))
        .ok_or_else(|| error_message("configuration").to_string())?;
    let mut child = std::process::Command::new(&helper)
        .arg("--authorize-for-client").current_dir(helper.parent().unwrap())
        .env_remove("OMSI_ACCESS_TOKEN").env_remove("OMSI_ACCESS_ORIGIN")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW: OAuth opens its own browser.
        .spawn().map_err(|_| error_message("configuration").to_string())?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill(); let _ = child.wait(); return Err(FAILED.into());
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take((LIMIT + 1) as u64).read_to_end(&mut bytes);
        let _ = tx.send((result.is_ok(), bytes));
    });
    let mut output = None;
    let status = loop {
        if output.is_none() { output = rx.try_recv().ok(); }
        if output.as_ref().is_some_and(|(ok, bytes)| !ok || bytes.len() > LIMIT) {
            let _ = child.kill(); let _ = child.wait();
            if let Some((_, bytes)) = output.as_mut() { bytes.fill(0); }
            return Err(FAILED.into());
        }
        if cancel.load(Ordering::Acquire) || started.elapsed() >= DEADLINE {
            let _ = child.kill(); let _ = child.wait();
            if let Some((_, bytes)) = output.as_mut() { bytes.fill(0); }
            return Err(if cancel.load(Ordering::Acquire) { CANCELLED.into() } else { "Discord sign-in timed out. Press Join to try again.".into() });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(_) => { let _ = child.kill(); let _ = child.wait(); return Err(FAILED.into()); }
        }
    };
    let Some((ok, mut bytes)) = output.or_else(|| rx.recv_timeout(Duration::from_secs(1)).ok()) else { return Err(FAILED.into()); };
    let result = if ok && !cancel.load(Ordering::Acquire) { parse_reply(&bytes, status.code(), SystemTime::now()) } else { Err(CANCELLED.into()) };
    bytes.fill(0);
    result
}

#[cfg(not(windows))]
pub fn authorize(_cancel: Arc<AtomicBool>) -> Result<Ticket, String> {
    Err("Joining the protected Tangenta server requires its Windows sign-in helper.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(expiry: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"game_token":"A".repeat(43), "expires_at":expiry, "discord_user_id":"12345678901234567", "display_name":"Player"})).unwrap()
    }

    #[test]
    fn dotnet_utc_expiry_and_clock_tolerance_keep_original_expiry() {
        let now = parse_utc("2026-10-03T21:00:00Z").unwrap();
        let exact = parse_utc("2026-10-04T05:00:00.0000000+00:00").unwrap();
        assert_eq!(parse_reply(&reply("2026-10-04T05:00:00.0000000+00:00"), Some(0), now).unwrap().expires_at, exact);
        assert!(parse_reply(&reply("2026-10-04T05:00:00.1730000+00:00"), Some(0), now).is_ok());
        assert!(parse_reply(&reply("2026-10-04T05:02:01Z"), Some(0), now).is_err());
        assert!(parse_reply(&reply("2026-10-03T21:00:00Z"), Some(0), now).is_err());
        assert!(parse_reply(&reply("2026-10-03T20:59:59Z"), Some(0), now).is_err());
    }

    #[test]
    fn ipc_is_bounded_exact_and_never_displays_helper_payload() {
        let now = parse_utc("2026-10-03T21:00:00Z").unwrap();
        let failure = br#"{"error_code":"denied","message":"secret-untrusted-response"}"#;
        assert_eq!(parse_reply(failure, Some(1), now).err().unwrap(), error_message("denied"));
        assert!(!parse_reply(failure, Some(1), now).err().unwrap().contains("secret"));
        assert!(parse_reply(&vec![b'x'; LIMIT + 1], Some(0), now).is_err());
        let mut json: serde_json::Value = serde_json::from_slice(&reply("2026-10-03T22:00:00Z")).unwrap();
        json["extra"] = true.into();
        assert!(parse_reply(&serde_json::to_vec(&json).unwrap(), Some(0), now).is_err());
        json.as_object_mut().unwrap().remove("extra");
        json["game_token"] = "ticket\nheader".into();
        assert!(parse_reply(&serde_json::to_vec(&json).unwrap(), Some(0), now).is_err());
    }

    #[test]
    fn utc_dates_reject_invalid_calendar_and_offsets() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
        assert!(parse_utc("2024-02-29T12:00:00Z").is_some());
        for value in ["2026-02-29T12:00:00Z", "2026-10-03T24:00:00Z", "2026-10-03T21:00:00+01:00", "2026-10-03T21:00:00.Z", "2026-10-03T21:00:00.00000000Z"] {
            assert!(parse_utc(value).is_none(), "{value}");
        }
    }

    #[test]
    fn cancelled_retry_never_waits_for_or_enters_a_busy_helper_slot() {
        let lock = Mutex::new(());
        let _previous = lock.lock().unwrap();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        assert_eq!(authorization_slot(&lock, &cancel, started, DEADLINE).err().unwrap(), CANCELLED);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn retry_waits_until_previous_helper_has_released_its_callback() {
        let lock = Arc::new(Mutex::new(()));
        let previous = lock.lock().unwrap();
        let retry_lock = lock.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let retry = std::thread::spawn(move || {
            let cancel = AtomicBool::new(false);
            let _slot = authorization_slot(&retry_lock, &cancel, Instant::now(), Duration::from_secs(2)).unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(previous); // the cancelled predecessor has killed/reaped its helper
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        retry.join().unwrap();
    }
}
