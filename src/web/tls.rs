// file: src/web/tls.rs
// version: 1.0.0
// guid: ab244890-9ac3-4859-b35f-689b2693820f
// last-edited: 2026-10-11

//! Serves the UI with this node's own CockroachDB certificate, found through
//! the CockroachDB unit's `--certs-dir`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Picks the certs dir: the override, else `--certs-dir` from the unit's
/// argv, resolved against `WorkingDirectory` (systemd's `/` if unset).
pub fn choose_certs_dir(
    override_dir: Option<&Path>,
    exec_start: &str,
    working_dir: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }
    let mut words = exec_start.split_whitespace();
    let mut found = None;
    while let Some(word) = words.next() {
        if let Some(value) = word.strip_prefix("--certs-dir=") {
            found = Some(value.to_string());
        } else if word == "--certs-dir" {
            found = words.next().map(str::to_string);
        }
    }
    let dir = PathBuf::from(found.ok_or_else(|| {
        "the CockroachDB unit has no --certs-dir; set CROACH_ROLLOUT_UI_CERTS_DIR".to_string()
    })?);
    if dir.is_absolute() {
        return Ok(dir);
    }
    let base = working_dir
        .map(|dir| dir.trim_start_matches(['-', '!']))
        .filter(|dir| !dir.is_empty())
        .unwrap_or("/");
    Ok(Path::new(base).join(dir))
}

pub fn resolve_certs_dir(override_dir: Option<&Path>, service: &str) -> Result<PathBuf, String> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }
    let output = Command::new("systemctl")
        .args([
            "show",
            "--property=ExecStart",
            "--property=WorkingDirectory",
            service,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run systemctl show: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemctl show {service} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut exec_start = "";
    let mut working_dir = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ExecStart=") {
            exec_start = value;
        } else if let Some(value) = line.strip_prefix("WorkingDirectory=") {
            working_dir = Some(value);
        }
    }
    choose_certs_dir(None, exec_start, working_dir).map_err(|error| format!("{service}: {error}"))
}

/// TLS 1.2 and 1.3 only, `ring` provider, HTTP/1.1. Accepts PKCS#1, PKCS#8
/// and SEC1 keys; `cockroach cert` writes PKCS#1.
pub fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<rustls::ServerConfig, String> {
    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("bad certificate PEM: {error}"))?;
    if certs.is_empty() {
        return Err("no certificate found in the PEM".to_string());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|error| format!("bad private key PEM: {error}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| format!("certificate and key do not form a usable pair: {error}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

pub fn load_server_config(dir: &Path) -> Result<rustls::ServerConfig, String> {
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))
    };
    server_config(&read("node.crt")?, &read("node.key")?)
        .map_err(|error| format!("{}: {error}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEC: &str = "{ path=/var/lib/cockroach-rollout-agent/bin/cockroach ; argv[]=/var/lib/cockroach-rollout-agent/bin/cockroach start --certs-dir=/var/lib/cockroach/certs --store=path=/var/lib/cockroach ; ignore_errors=no ; start_time=[n/a] }";

    #[test]
    fn certs_dir_comes_from_the_unit() {
        assert_eq!(
            choose_certs_dir(None, EXEC, None).unwrap(),
            PathBuf::from("/var/lib/cockroach/certs")
        );
        let spaced = EXEC.replace(
            "--certs-dir=/var/lib/cockroach/certs",
            "--certs-dir /srv/certs",
        );
        assert_eq!(
            choose_certs_dir(None, &spaced, None).unwrap(),
            PathBuf::from("/srv/certs")
        );
    }

    #[test]
    fn relative_certs_dir_resolves_against_the_working_directory() {
        let relative = EXEC.replace("/var/lib/cockroach/certs", "certs");
        assert_eq!(
            choose_certs_dir(None, &relative, Some("/mnt/bigdata/cockroach")).unwrap(),
            PathBuf::from("/mnt/bigdata/cockroach/certs")
        );
        assert_eq!(
            choose_certs_dir(None, &relative, None).unwrap(),
            PathBuf::from("/certs"),
            "systemd starts in / without WorkingDirectory"
        );
        assert_eq!(
            choose_certs_dir(None, &relative, Some("-/opt/crdb")).unwrap(),
            PathBuf::from("/opt/crdb/certs"),
            "the optional-directory prefix is not part of the path"
        );
    }

    #[test]
    fn override_wins_and_missing_flag_is_explained() {
        assert_eq!(
            choose_certs_dir(Some(Path::new("/etc/ui-certs")), EXEC, None).unwrap(),
            PathBuf::from("/etc/ui-certs")
        );
        let error = choose_certs_dir(None, "{ path=/x ; argv[]=/x start ; }", None).unwrap_err();
        assert!(error.contains("CROACH_ROLLOUT_UI_CERTS_DIR"), "{error}");
    }

    #[test]
    fn server_config_loads_a_pem_pair_and_rejects_garbage() {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let config = server_config(
            cert.cert.pem().as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(server_config(b"not a cert", b"not a key").is_err());
        let dir = tempfile::tempdir().unwrap();
        let error = load_server_config(dir.path()).unwrap_err();
        assert!(error.contains("node.crt"), "{error}");
    }
}
