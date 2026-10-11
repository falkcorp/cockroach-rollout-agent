// file: src/web/auth.rs
// version: 1.1.0
// guid: 3b5291c0-75af-4721-8be7-20704b656704
// last-edited: 2026-10-11

//! Password, session and CSRF primitives for the web UI. One shared
//! password; argon2id hashes; random session tokens stored only as SHA-256.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
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

#[derive(Default)]
struct IpRecord {
    failures: Vec<Instant>,
    /// Password checks admitted and not yet settled.
    in_flight: usize,
}

/// Per-IP failed-login counter, in memory on each node. A speed bump against
/// guessing, not a distributed lockout. Checks still running count against
/// the limit, so concurrent requests cannot slip past it.
#[derive(Default)]
pub struct LoginLimiter {
    records: Mutex<HashMap<IpAddr, IpRecord>>,
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    fn records(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, IpRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Admits one password check for `ip`, or `None` when recent failures
    /// plus checks in flight already reach `MAX_FAILURES`.
    pub fn try_begin(self: &Arc<Self>, ip: IpAddr, now: Instant) -> Option<LoginAttempt> {
        let mut records = self.records();
        records.retain(|_, record| {
            record
                .failures
                .retain(|time| now.duration_since(*time) < FAILURE_WINDOW);
            !record.failures.is_empty() || record.in_flight > 0
        });
        let record = records.entry(ip).or_default();
        if record.failures.len() + record.in_flight >= MAX_FAILURES {
            return None;
        }
        record.in_flight += 1;
        Some(LoginAttempt {
            limiter: Arc::clone(self),
            ip,
        })
    }
}

/// One admitted password check. Dropping it without a verdict (an error or
/// a cancelled request) frees its slot without counting a failure.
pub struct LoginAttempt {
    limiter: Arc<LoginLimiter>,
    ip: IpAddr,
}

impl LoginAttempt {
    pub fn failed(self, now: Instant) {
        self.limiter
            .records()
            .entry(self.ip)
            .or_default()
            .failures
            .push(now);
    }

    pub fn succeeded(self) {
        if let Some(record) = self.limiter.records().get_mut(&self.ip) {
            record.failures.clear();
        }
    }
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        if let Some(record) = self.limiter.records().get_mut(&self.ip) {
            record.in_flight = record.in_flight.saturating_sub(1);
        }
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
        let limiter = Arc::new(LoginLimiter::new());
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let start = Instant::now();
        for _ in 0..5 {
            limiter.try_begin(ip, start).unwrap().failed(start);
        }
        assert!(limiter.try_begin(ip, start).is_none());
        assert!(
            limiter.try_begin(other, start).is_some(),
            "lockout is per IP"
        );
        assert!(
            limiter
                .try_begin(ip, start + Duration::from_secs(61))
                .is_some(),
            "the window slides"
        );
    }

    #[test]
    fn checks_in_flight_count_against_the_limit() {
        let limiter = Arc::new(LoginLimiter::new());
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let now = Instant::now();
        let running: Vec<_> = (0..5)
            .map(|_| limiter.try_begin(ip, now).unwrap())
            .collect();
        assert!(
            limiter.try_begin(ip, now).is_none(),
            "five unsettled checks fill the limit"
        );
        drop(running);
        assert!(
            limiter.try_begin(ip, now).is_some(),
            "abandoned checks free their slots without counting as failures"
        );
    }

    #[test]
    fn success_clears_earlier_failures() {
        let limiter = Arc::new(LoginLimiter::new());
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let now = Instant::now();
        for _ in 0..4 {
            limiter.try_begin(ip, now).unwrap().failed(now);
        }
        limiter.try_begin(ip, now).unwrap().succeeded();
        for _ in 0..5 {
            limiter.try_begin(ip, now).unwrap().failed(now);
        }
        assert!(limiter.try_begin(ip, now).is_none());
    }
}
