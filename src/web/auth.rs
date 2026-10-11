// file: src/web/auth.rs
// version: 1.0.0
// guid: 3b5291c0-75af-4721-8be7-20704b656704
// last-edited: 2026-10-11

//! Password, session and CSRF primitives for the web UI. One shared
//! password; argon2id hashes; random session tokens stored only as SHA-256.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const MIN_PASSWORD_CHARS: usize = 12;
pub const SESSION_COOKIE: &str = "crra_session";
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// argon2id with the crate defaults and a fresh random salt.
pub fn hash_password(password: &str) -> Result<String, String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!(
            "the UI password must be at least {MIN_PASSWORD_CHARS} characters"
        ));
    }
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| format!("could not hash the password: {error}"))
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

pub fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Sessions are stored by this hash, so a leaked `ui_sessions` table does
/// not hand out live cookies.
pub fn token_sha256(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn csrf_matches(expected: &str, given: &str) -> bool {
    !expected.is_empty()
        && expected.len() == given.len()
        && bool::from(expected.as_bytes().ct_eq(given.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub csrf_token: String,
    pub expires_unix: i64,
}

pub fn session_live(session: &Session, now_unix: i64) -> bool {
    now_unix < session.expires_unix
}

/// Value of cookie `name` in one `Cookie:` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').map(str::trim).find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Per-IP failed-login counter, in memory on each node. A speed bump against
/// guessing, not a distributed lockout.
#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        failures.get(&ip).is_some_and(|times| {
            times
                .iter()
                .filter(|time| now.duration_since(**time) < FAILURE_WINDOW)
                .count()
                >= MAX_FAILURES
        })
    }

    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        failures.retain(|_, times| {
            times.retain(|time| now.duration_since(*time) < FAILURE_WINDOW);
            !times.is_empty()
        });
        failures.entry(ip).or_default().push(now);
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn passwords_hash_and_verify() {
        assert!(hash_password("too short").is_err());
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("correct horse batterY", &hash));
        assert!(!verify_password(
            "correct horse battery",
            "not a phc string"
        ));
        assert_ne!(
            hash,
            hash_password("correct horse battery").unwrap(),
            "salts differ"
        );
    }

    #[test]
    fn tokens_are_random_hex_and_hashed_for_storage() {
        let token = random_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|character| character.is_ascii_hexdigit()));
        assert_ne!(token, random_token());
        assert_eq!(token_sha256("abc").len(), 64);
        assert_ne!(token_sha256(&token), token);
    }

    #[test]
    fn csrf_comparison_requires_an_exact_match() {
        assert!(csrf_matches("abc123", "abc123"));
        assert!(!csrf_matches("abc123", "abc124"));
        assert!(!csrf_matches("abc123", "abc12"));
        assert!(!csrf_matches("", ""), "an empty token never matches");
    }

    #[test]
    fn sessions_expire_at_their_deadline() {
        let session = Session {
            csrf_token: "c".to_string(),
            expires_unix: 1_000,
        };
        assert!(session_live(&session, 999));
        assert!(!session_live(&session, 1_000));
    }

    #[test]
    fn cookies_are_found_by_exact_name() {
        let header = "theme=dark; crra_session=abc; crra_session_old=zzz";
        assert_eq!(cookie_value(header, "crra_session"), Some("abc"));
        assert_eq!(cookie_value(header, "crra_flash"), None);
    }

    #[test]
    fn five_failures_lock_an_ip_for_a_minute() {
        let limiter = LoginLimiter::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let start = Instant::now();
        for _ in 0..4 {
            limiter.record_failure(ip, start);
        }
        assert!(!limiter.blocked(ip, start));
        limiter.record_failure(ip, start);
        assert!(limiter.blocked(ip, start));
        assert!(!limiter.blocked(other, start), "lockout is per IP");
        assert!(
            !limiter.blocked(ip, start + Duration::from_secs(61)),
            "the window slides"
        );
        limiter.record_success(ip);
        assert!(!limiter.blocked(ip, start));
    }
}
