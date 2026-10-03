//! Short-lived access credentials, scoped to one HTTPS origin.
//!
//! A launcher can authorize a join after it starts, without changing the process
//! environment. Credentials stay in memory and expire before another request uses
//! them. A cleared or expired runtime credential also disables the legacy inherited
//! environment credential for that origin, so cancelling a join cannot revive it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Origin {
    secure: bool,
    authority: String,
    loopback: bool,
}

/// HTTP and WebSocket schemes describe the same origin when their security and
/// host/port match. Paths never widen the scope of a credential.
fn access_origin(url: &str) -> Option<Origin> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') || !authority.is_ascii() {
        return None;
    }
    let default = if secure { 443 } else { 80 };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, tail) = bracketed.split_once(']')?;
        let ip = host.parse::<std::net::Ipv6Addr>().ok()?;
        let port = if tail.is_empty() { default } else { tail.strip_prefix(':')?.parse::<u16>().ok()? };
        (format!("[{ip}]"), port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok()?),
            None => (authority, default),
        };
        if host.is_empty() || !host.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-')) {
            return None;
        }
        (host.to_ascii_lowercase(), port)
    };
    if port == 0 { return None; }
    let loopback = host == "[::1]" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback());
    Some(Origin { secure, authority: format!("{host}:{port}"), loopback })
}

fn runtime_origin(origin: &str) -> Result<Origin, String> {
    let (scheme, authority) = origin.trim().split_once("://").ok_or("invalid access origin")?;
    // Installing a credential accepts an origin, not an endpoint URL. Only HTTPS
    // is accepted here; the legacy API retains its local-loopback test support.
    if !scheme.eq_ignore_ascii_case("https") || authority.strip_suffix('/').unwrap_or(authority).contains(['/', '?', '#']) {
        return Err("access credentials require an HTTPS origin".into());
    }
    access_origin(origin).filter(|o| o.secure).ok_or_else(|| "invalid access origin".into())
}

fn validate_token(token: &str) -> Result<(), String> {
    // Errors must not echo a bearer token or an untrusted helper response.
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|c| c.is_ascii_graphic()) {
        return Err("invalid access credential".into());
    }
    Ok(())
}

fn scoped_access_token(url: &str, origin: Option<&str>, token: Option<&str>) -> Result<Option<String>, String> {
    let Some(token) = token.filter(|t| !t.is_empty()) else { return Ok(None) };
    let configured = origin.and_then(access_origin).ok_or("access origin is not configured")?;
    let Some(requested) = access_origin(url) else { return Ok(None) };
    if configured != requested {
        return Ok(None);
    }
    if !requested.secure && !requested.loopback {
        return Err("access credentials require HTTPS or WSS".into());
    }
    validate_token(token)?;
    Ok(Some(token.to_owned()))
}

#[cfg(test)]
pub(crate) fn scoped_access_header(url: &str, origin: Option<&str>, token: Option<&str>) -> Result<Option<String>, String> {
    scoped_access_token(url, origin, token).map(|token| token.map(|token| format!("Bearer {token}")))
}

// Deliberately no Debug/Serialize implementation on a credential or this cache.
enum Entry {
    Active { token: String, expires_at: SystemTime },
    Cleared,
}

#[derive(Default)]
struct AccessCache {
    entries: HashMap<Origin, Entry>,
}

impl AccessCache {
    fn install(&mut self, origin: &str, token: String, expires_at: SystemTime, now: SystemTime) -> Result<(), String> {
        let origin = runtime_origin(origin)?;
        validate_token(&token)?;
        if expires_at <= now {
            return Err("access credential has expired".into());
        }
        self.entries.insert(origin, Entry::Active { token, expires_at });
        Ok(())
    }

    fn clear(&mut self, origin: &str) -> Result<(), String> {
        self.entries.insert(runtime_origin(origin)?, Entry::Cleared);
        Ok(())
    }

    fn token_for(&mut self, url: &str, now: SystemTime, legacy_origin: Option<&str>, legacy_token: Option<&str>) -> Result<Option<String>, String> {
        if let Some(entry) = access_origin(url).and_then(|origin| self.entries.get_mut(&origin)) {
            return match entry {
                Entry::Active { token, expires_at } if *expires_at > now => Ok(Some(token.clone())),
                Entry::Active { .. } => {
                    // Drop the expired secret and retain a tombstone: the inherited
                    // old credential must not become effective after expiry.
                    *entry = Entry::Cleared;
                    Ok(None)
                }
                Entry::Cleared => Ok(None),
            };
        }
        scoped_access_token(url, legacy_origin, legacy_token)
    }
}

static RUNTIME_ACCESS: OnceLock<Mutex<AccessCache>> = OnceLock::new();

fn cache() -> &'static Mutex<AccessCache> {
    RUNTIME_ACCESS.get_or_init(|| Mutex::new(AccessCache::default()))
}

/// Whether an endpoint belongs to the configured HTTPS origin. HTTP downgrade,
/// another port, userinfo and a lookalike hostname do not match. A trusted local
/// helper may be selected using this check; remote metadata must not select one.
pub fn matches_origin(url: &str, origin: &str) -> bool {
    let Ok(configured) = runtime_origin(origin) else { return false };
    access_origin(url).is_some_and(|requested| configured == requested)
}

/// Install a short-lived credential returned by a trusted local authorization
/// helper. `origin` must be an HTTPS origin; `expires_at` is the server's expiry.
/// The token is never written to the environment, configuration or a log.
pub fn install(origin: &str, token: String, expires_at: SystemTime) -> Result<(), String> {
    cache().lock().map_err(|_| "access credential cache is unavailable")?.install(origin, token, expires_at, SystemTime::now())
}

/// Revoke the credential for an origin, including any legacy inherited token for
/// that origin. Use this when authorization is cancelled or a server rejects it.
pub fn clear(origin: &str) -> Result<(), String> {
    cache().lock().map_err(|_| "access credential cache is unavailable")?.clear(origin)
}

/// Return the current token only for a matching HTTPS/WSS origin. This is also
/// used to set the environment of a newly spawned game process for that join.
/// Callers must not log the returned value or pass it in command-line arguments.
pub fn token_for(url: &str) -> Result<Option<String>, String> {
    // Compatibility with the auth-first launcher remains read-only. Runtime
    // authorization takes precedence and never mutates a multithreaded process's
    // environment.
    let legacy_token = std::env::var("OMSI_ACCESS_TOKEN").ok();
    let legacy_origin = std::env::var("OMSI_ACCESS_ORIGIN").ok();
    cache().lock().map_err(|_| "access credential cache is unavailable")?.token_for(url, SystemTime::now(), legacy_origin.as_deref(), legacy_token.as_deref())
}

/// Whether a join has a currently usable credential, without exposing it to UI.
pub fn has_valid_credentials(url: &str) -> Result<bool, String> {
    token_for(url).map(|token| token.is_some())
}

pub(crate) fn authorization_for(url: &str) -> Result<Option<String>, String> {
    token_for(url).map(|token| token.map(|token| format!("Bearer {token}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn now() -> SystemTime { UNIX_EPOCH + Duration::from_secs(10_000) }

    #[test]
    fn runtime_ticket_matches_only_its_https_websocket_origin() {
        let mut cache = AccessCache::default();
        cache.install("https://PRIVATE.example:443/", "runtime-ticket".into(), now() + Duration::from_secs(60), now()).unwrap();
        for url in ["https://private.example/status", "wss://PRIVATE.example/ws", "https://private.example:443/tcp"] {
            assert_eq!(cache.token_for(url, now(), None, None).unwrap().as_deref(), Some("runtime-ticket"));
        }
        for url in ["https://other.example/status", "https://private.example.evil/status", "https://private.example:8443/status", "http://private.example/status", "wss://private.example@evil.example/ws", "file://private.example/status"] {
            assert!(cache.token_for(url, now(), None, None).unwrap().is_none(), "credential left its origin");
        }
    }

    #[test]
    fn expired_runtime_ticket_does_not_revive_inherited_token() {
        let mut cache = AccessCache::default();
        let origin = "https://private.example";
        let expiry = now() + Duration::from_secs(60);
        cache.install(origin, "runtime-ticket".into(), expiry, now()).unwrap();
        assert_eq!(cache.token_for(origin, now(), Some(origin), Some("legacy-ticket")).unwrap().as_deref(), Some("runtime-ticket"));
        assert!(cache.token_for(origin, expiry, Some(origin), Some("legacy-ticket")).unwrap().is_none());
        assert!(cache.token_for(origin, expiry + Duration::from_secs(1), Some(origin), Some("legacy-ticket")).unwrap().is_none());
        assert!(matches!(cache.entries.get(&runtime_origin(origin).unwrap()), Some(Entry::Cleared)));
    }

    #[test]
    fn clearing_cancelled_join_blocks_legacy_token_until_new_authorization() {
        let mut cache = AccessCache::default();
        let origin = "https://private.example";
        assert_eq!(cache.token_for(origin, now(), Some(origin), Some("legacy-ticket")).unwrap().as_deref(), Some("legacy-ticket"));
        cache.clear(origin).unwrap();
        assert!(cache.token_for(origin, now(), Some(origin), Some("legacy-ticket")).unwrap().is_none());
        cache.install(origin, "fresh-ticket".into(), now() + Duration::from_secs(60), now()).unwrap();
        assert_eq!(cache.token_for("wss://private.example/ws", now(), Some(origin), Some("legacy-ticket")).unwrap().as_deref(), Some("fresh-ticket"));
    }

    #[test]
    fn clearing_one_origin_preserves_other_origins() {
        let mut cache = AccessCache::default();
        for origin in ["https://first.example", "https://first.example:8443", "https://second.example"] {
            cache.install(origin, "runtime-ticket".into(), now() + Duration::from_secs(60), now()).unwrap();
        }
        cache.clear("https://FIRST.example:443").unwrap();
        assert!(cache.token_for("https://first.example", now(), None, None).unwrap().is_none());
        assert!(cache.token_for("https://first.example:8443", now(), None, None).unwrap().is_some());
        assert!(cache.token_for("https://second.example", now(), None, None).unwrap().is_some());
    }

    #[test]
    fn runtime_install_rejects_invalid_origins_expiry_and_header_injection() {
        let mut cache = AccessCache::default();
        for origin in ["http://private.example", "wss://private.example", "https://private.example/path", "https://private.example//", "https://private.example?query", "https://private.example#fragment", "https://user@private.example", "https://private.example:0", "https://private.example:bad"] {
            assert!(cache.install(origin, "ticket".into(), now() + Duration::from_secs(60), now()).is_err());
        }
        assert!(cache.install("https://private.example", "ticket".into(), now(), now()).is_err());
        for token in ["".to_owned(), "SECRET\r\nInjected: yes".to_owned(), "a".repeat(4097)] {
            let error = cache.install("https://private.example", token, now() + Duration::from_secs(60), now()).unwrap_err();
            assert!(!error.contains("SECRET"));
        }
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn helper_origin_selection_uses_the_same_scope_as_access_headers() {
        let origin = "https://private.example";
        for url in ["https://PRIVATE.example:443/", "https://private.example/players", "wss://private.example/ws"] {
            assert!(matches_origin(url, origin));
        }
        for url in ["http://private.example", "https://private.example:8443", "https://private.example.evil", "https://private.example@evil.example", "https://prívate.example", "https://private.example:bad"] {
            assert!(!matches_origin(url, origin));
        }
        assert!(!matches_origin("https://private.example/ws", "http://private.example"));
        assert!(!matches_origin("https://private.example/ws", "https://private.example/path"));
    }

    #[test]
    fn public_runtime_api_supplies_shared_access_headers_and_child_token() {
        let origin = "https://runtime-api-test.example";
        install(origin, "runtime-test-ticket".into(), SystemTime::now() + Duration::from_secs(60)).unwrap();
        assert!(has_valid_credentials("wss://runtime-api-test.example/ws").unwrap());
        assert_eq!(authorization_for("https://runtime-api-test.example/players").unwrap().as_deref(), Some("Bearer runtime-test-ticket"));
        assert_eq!(token_for("wss://runtime-api-test.example/tcp").unwrap().as_deref(), Some("runtime-test-ticket"));
        assert!(token_for("https://other-runtime-api-test.example").unwrap().is_none());
        clear(origin).unwrap();
        assert!(!has_valid_credentials(origin).unwrap());
        assert!(authorization_for("wss://runtime-api-test.example/ws").unwrap().is_none());
    }
}
