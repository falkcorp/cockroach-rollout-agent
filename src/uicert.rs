// file: src/uicert.rs
// version: 1.0.0
// guid: e98e49c8-344f-4728-933e-19194e468320
// last-edited: 2026-10-12

//! Certificates for the web UI. A private "Rollout UI CA", name-constrained
//! to the cluster's hosts and IPs, signs short-lived leaf certificates. A
//! leaf is staged in the coordination DB, approved by a person, and then
//! every agent installs it and hot-reloads its listener. The CA key never
//! enters the DB.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use postgres::Client;
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PublicKeyData, SanType,
};
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::{AppError, Cli, sql_ident};

pub const CA_DAYS: i64 = 3650;
/// Under Apple's 398-day ceiling for TLS server certificates.
pub const LEAF_DAYS: i64 = 397;
pub const RENEW_BEFORE_DAYS: i64 = 30;
/// Upload fields larger than this are refused before parsing.
pub const MAX_PEM_BYTES: usize = 16 * 1024;

/// The names a UI certificate covers: every agent's short hostname and every
/// node's gossip IP.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hosts {
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
}

impl Hosts {
    fn push(&mut self, name: &str) {
        let name = name.trim().trim_start_matches('[').trim_end_matches(']');
        if name.is_empty() || name.eq_ignore_ascii_case("localhost") {
            return;
        }
        match name.parse::<IpAddr>() {
            Ok(ip) if !ip.is_loopback() => self.ips.push(ip),
            Ok(_) => {}
            Err(_) => self.dns.push(name.to_ascii_lowercase()),
        }
    }

    fn tidy(mut self) -> Self {
        self.dns.sort();
        self.dns.dedup();
        self.ips.sort();
        self.ips.dedup();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.dns.is_empty() && self.ips.is_empty()
    }

    pub fn describe(&self) -> String {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(IpAddr::to_string))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `agent_ids` look like `host:arch`; `addrs` like `ip:port` or `host:port`.
pub fn hosts_from(agent_ids: &[String], addrs: &[String], extra: &[String]) -> Hosts {
    let mut hosts = Hosts::default();
    for id in agent_ids {
        hosts.push(id.split(':').next().unwrap_or(""));
    }
    for addr in addrs {
        let host = match addr.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
            _ => addr.as_str(),
        };
        hosts.push(host);
    }
    for name in extra {
        hosts.push(name);
    }
    hosts.tidy()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pem {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub fingerprint: String,
    pub subject: String,
    pub issuer: String,
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
    pub not_before: i64,
    pub not_after: i64,
}

impl CertInfo {
    pub fn hosts(&self) -> Hosts {
        Hosts {
            dns: self.dns.clone(),
            ips: self.ips.clone(),
        }
        .tidy()
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn first_der(cert_pem: &str) -> Result<CertificateDer<'static>, String> {
    CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .ok_or_else(|| "no certificate in the PEM".to_string())?
        .map_err(|error| format!("bad certificate PEM: {error}"))
}

pub fn fingerprint_der(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

fn days(count: i64) -> time::Duration {
    time::Duration::days(count)
}

/// A self-signed CA allowed to sign only for `hosts`. macOS and webpki both
/// refuse a leaf it signs for any other name.
pub fn generate_ca(hosts: &Hosts, now: OffsetDateTime) -> Result<Pem, String> {
    if hosts.is_empty() {
        return Err("no hosts to constrain the CA to".to_string());
    }
    let key = KeyPair::generate().map_err(text)?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::OrganizationName, "cockroach-rollout-agent");
    name.push(DnType::CommonName, "Rollout UI CA");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = now - days(1);
    params.not_after = now + days(CA_DAYS);
    let mut permitted: Vec<GeneralSubtree> = hosts
        .dns
        .iter()
        .map(|dns| GeneralSubtree::DnsName(dns.clone()))
        .collect();
    for ip in &hosts.ips {
        permitted.push(GeneralSubtree::IpAddress(match ip {
            IpAddr::V4(v4) => CidrSubnet::from_v4_prefix(v4.octets(), 32),
            IpAddr::V6(v6) => CidrSubnet::from_v6_prefix(v6.octets(), 128),
        }));
    }
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: permitted,
        excluded_subtrees: Vec::new(),
    });
    let cert = params.self_signed(&key).map_err(text)?;
    Ok(Pem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// A serverAuth leaf for every name and IP in `hosts`, signed by `ca`.
pub fn issue_leaf(
    ca: &Pem,
    hosts: &Hosts,
    now: OffsetDateTime,
    valid_days: i64,
) -> Result<Pem, String> {
    if hosts.is_empty() {
        return Err("no hosts for the certificate".to_string());
    }
    let ca_key = KeyPair::from_pem(&ca.key).map_err(|error| format!("bad CA key: {error}"))?;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert, ca_key)
        .map_err(|error| format!("bad CA certificate: {error}"))?;
    let key = KeyPair::generate().map_err(text)?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "cockroach-rollout-agent UI");
    params.distinguished_name = name;
    params.subject_alt_names = hosts
        .dns
        .iter()
        .map(|dns| {
            dns.clone()
                .try_into()
                .map(SanType::DnsName)
                .map_err(|error| format!("bad DNS name {dns}: {error}"))
        })
        .chain(hosts.ips.iter().map(|ip| Ok(SanType::IpAddress(*ip))))
        .collect::<Result<_, _>>()?;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = now - days(1);
    params.not_after = now + days(valid_days);
    let cert = params.signed_by(&key, &issuer).map_err(text)?;
    Ok(Pem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

pub fn inspect(cert_pem: &str) -> Result<CertInfo, String> {
    let der = first_der(cert_pem)?;
    let (_, cert) =
        X509Certificate::from_der(&der).map_err(|error| format!("bad certificate: {error}"))?;
    let mut dns = Vec::new();
    let mut ips = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(name) => dns.push(name.to_string()),
                GeneralName::IPAddress(bytes) => {
                    if let Some(ip) = ip_from_bytes(bytes) {
                        ips.push(ip);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(CertInfo {
        fingerprint: fingerprint_der(&der),
        subject: cert.subject().to_string(),
        issuer: cert.issuer().to_string(),
        dns,
        ips,
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
    })
}

fn ip_from_bytes(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}

/// The hosts a CA is constrained to, so a renewal covers exactly what the
/// CA may sign.
pub fn ca_hosts(ca_pem: &str) -> Result<Hosts, String> {
    let der = first_der(ca_pem)?;
    let (_, cert) =
        X509Certificate::from_der(&der).map_err(|error| format!("bad CA certificate: {error}"))?;
    let constraints = cert
        .name_constraints()
        .map_err(|error| format!("bad name constraints: {error}"))?
        .ok_or_else(|| "the CA has no name constraints".to_string())?;
    let mut hosts = Hosts::default();
    for subtree in constraints.value.permitted_subtrees.iter().flatten() {
        match &subtree.base {
            GeneralName::DNSName(name) => hosts.dns.push(name.to_string()),
            // A constraint is the address followed by its mask.
            GeneralName::IPAddress(bytes) => {
                if let Some(ip) = ip_from_bytes(&bytes[..bytes.len() / 2]) {
                    hosts.ips.push(ip);
                }
            }
            _ => {}
        }
    }
    Ok(hosts.tidy())
}

/// Checks that `key_pem` belongs to `cert_pem`, that the cert chains to
/// `ca_pem` (name constraints included), and that it is valid at `now_unix`.
pub fn validate(
    cert_pem: &str,
    key_pem: &str,
    ca_pem: &str,
    now_unix: i64,
) -> Result<CertInfo, String> {
    if cert_pem.len() > MAX_PEM_BYTES || key_pem.len() > MAX_PEM_BYTES {
        return Err(format!("PEM fields are limited to {MAX_PEM_BYTES} bytes"));
    }
    let info = inspect(cert_pem)?;
    let der = first_der(cert_pem)?;
    let (_, cert) = X509Certificate::from_der(&der).map_err(text)?;
    let key = KeyPair::from_pem(key_pem).map_err(|error| format!("bad private key: {error}"))?;
    if key.subject_public_key_info() != cert.tbs_certificate.subject_pki.raw {
        return Err("the private key does not belong to the certificate".to_string());
    }

    let mut roots = RootCertStore::empty();
    roots
        .add(first_der(ca_pem)?)
        .map_err(|error| format!("bad CA certificate: {error}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(text)?;
    let name = info
        .ips
        .first()
        .map(|ip| ServerName::IpAddress((*ip).into()))
        .or_else(|| {
            info.dns
                .first()
                .and_then(|dns| ServerName::try_from(dns.clone()).ok())
        })
        .ok_or_else(|| "the certificate names no host or IP".to_string())?;
    let now = UnixTime::since_unix_epoch(Duration::from_secs(now_unix.max(0) as u64));
    verifier
        .verify_server_cert(&der, &[], &name, &[], now)
        .map_err(|error| format!("the certificate does not verify under the UI CA: {error}"))?;
    Ok(info)
}

/// Whether this node may serve `info`: its hostname or one of its IPs is in
/// the SANs.
pub fn covers(info: &CertInfo, hostname: &str, ips: &[IpAddr]) -> bool {
    info.dns
        .iter()
        .any(|dns| dns.eq_ignore_ascii_case(hostname))
        || ips.iter().any(|ip| info.ips.contains(ip))
}

pub fn renew_due(not_after_unix: i64, now_unix: i64, before_days: i64) -> bool {
    not_after_unix - now_unix <= before_days * 86_400
}

/// A staged or approved certificate as stored, without its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiCertRow {
    pub info: CertInfo,
    pub status: String,
    pub created_by: String,
    pub created_at: String,
    pub decided_by: Option<String>,
}

/// A stored certificate with its key and the CA it chains to.
#[derive(Debug, Clone)]
pub struct Material {
    pub fingerprint: String,
    pub pem: Pem,
    pub chain_pem: String,
}

/// Validates and stores a certificate as `staged`, superseding any other
/// staged one. Nothing serves it until a person approves it.
pub fn stage(
    cli: &Cli,
    client: &mut Client,
    pem: &Pem,
    chain_pem: &str,
    by: &str,
    now_unix: i64,
) -> Result<CertInfo, AppError> {
    let info = validate(&pem.cert, &pem.key, chain_pem, now_unix).map_err(AppError::Message)?;
    let schema = sql_ident(&cli.schema)?;
    let mut tx = client.transaction()?;
    tx.execute(
        &format!(
            "UPDATE {schema}.ui_certs SET status = 'rejected', decided_by = 'superseded', \
             decided_at = now() WHERE status = 'staged' AND fingerprint <> $1"
        ),
        &[&info.fingerprint],
    )?;
    let existing = tx.query(
        &format!("SELECT status FROM {schema}.ui_certs WHERE fingerprint = $1"),
        &[&info.fingerprint],
    )?;
    if let Some(row) = existing.first() {
        // Dropping the transaction rolls back the supersede above.
        let status: String = row.get(0);
        return Err(AppError::Message(format!(
            "certificate {} is already stored as {status}; stage a new one instead",
            info.fingerprint
        )));
    }
    tx.execute(
        &format!(
            "INSERT INTO {schema}.ui_certs \
                (fingerprint, cert_pem, key_pem, chain_pem, status, not_before, not_after, created_by) \
             VALUES ($1, $2, $3, $4, 'staged', $5, $6, $7)"
        ),
        &[
            &info.fingerprint,
            &pem.cert,
            &pem.key,
            &chain_pem,
            &info.not_before,
            &info.not_after,
            &by,
        ],
    )?;
    tx.commit()?;
    Ok(info)
}

/// The approved and staged certificates, newest first.
pub fn rows(cli: &Cli, client: &mut Client) -> Result<Vec<UiCertRow>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    client
        .query(
            &format!(
                "SELECT cert_pem, status, created_by, created_at::STRING, decided_by \
                 FROM {schema}.ui_certs WHERE status IN ('approved', 'staged') \
                 ORDER BY created_at DESC"
            ),
            &[],
        )?
        .iter()
        .map(|row| {
            let cert_pem: String = row.get(0);
            Ok(UiCertRow {
                info: inspect(&cert_pem).map_err(AppError::Message)?,
                status: row.get(1),
                created_by: row.get(2),
                created_at: row.get(3),
                decided_by: row.get(4),
            })
        })
        .collect()
}

pub fn material(
    cli: &Cli,
    client: &mut Client,
    fingerprint: Option<&str>,
    status: &str,
) -> Result<Option<Material>, AppError> {
    let schema = sql_ident(&cli.schema)?;
    let rows = client.query(
        &format!(
            "SELECT fingerprint, cert_pem, key_pem, chain_pem FROM {schema}.ui_certs \
             WHERE status = $1 AND ($2::STRING IS NULL OR fingerprint = $2) \
             ORDER BY created_at DESC LIMIT 1"
        ),
        &[&status, &fingerprint],
    )?;
    Ok(rows.first().map(|row| Material {
        fingerprint: row.get(0),
        pem: Pem {
            cert: row.get(1),
            key: row.get(2),
        },
        chain_pem: row.get(3),
    }))
}

/// Any stored certificate by fingerprint, whatever its status.
pub fn material_any(
    cli: &Cli,
    client: &mut Client,
    fingerprint: &str,
) -> Result<Option<Material>, AppError> {
    for status in ["approved", "staged", "retired", "rejected"] {
        if let Some(found) = material(cli, client, Some(fingerprint), status)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Approves or rejects the staged certificate `fingerprint`. Approval
/// retires the previous approved certificate in the same transaction, so
/// exactly one is ever approved.
pub fn decide(
    cli: &Cli,
    client: &mut Client,
    fingerprint: &str,
    approve: bool,
    by: &str,
) -> Result<(), AppError> {
    let schema = sql_ident(&cli.schema)?;
    let mut tx = client.transaction()?;
    let staged = tx.query(
        &format!("SELECT 1 FROM {schema}.ui_certs WHERE fingerprint = $1 AND status = 'staged'"),
        &[&fingerprint],
    )?;
    if staged.is_empty() {
        return Err(AppError::Message(
            "that certificate is not the staged one; reload and try again".to_string(),
        ));
    }
    if approve {
        tx.execute(
            &format!("UPDATE {schema}.ui_certs SET status = 'retired' WHERE status = 'approved'"),
            &[],
        )?;
    }
    tx.execute(
        &format!(
            "UPDATE {schema}.ui_certs SET status = $2, decided_by = $3, decided_at = now() \
             WHERE fingerprint = $1 AND status = 'staged'"
        ),
        &[
            &fingerprint,
            &if approve { "approved" } else { "rejected" },
            &by,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn hosts() -> Hosts {
        hosts_from(
            &[
                "len-serv-001:amd64".to_string(),
                "unimatrixzero:amd64".to_string(),
            ],
            &[
                "172.16.3.92:36257".to_string(),
                "172.16.2.30:36257".to_string(),
            ],
            &[],
        )
    }

    #[test]
    fn hosts_come_from_agent_ids_and_gossip_addresses() {
        let hosts = hosts_from(
            &[
                "len-serv-001:amd64".to_string(),
                "Len-Serv-001:arm64".to_string(),
            ],
            &[
                "172.16.3.92:36257".to_string(),
                "len-serv-002:36257".to_string(),
                "127.0.0.1:26257".to_string(),
                "localhost:26257".to_string(),
            ],
            &["ui.example".to_string()],
        );
        assert_eq!(hosts.dns, ["len-serv-001", "len-serv-002", "ui.example"]);
        assert_eq!(hosts.ips, ["172.16.3.92".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn ca_constrains_names_and_ips() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        assert_eq!(ca_hosts(&ca.cert).unwrap(), hosts());
        let info = inspect(&ca.cert).unwrap();
        assert!(info.subject.contains("Rollout UI CA"), "{}", info.subject);
    }

    #[test]
    fn leaf_validates_under_its_ca_and_not_under_another() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        let leaf = issue_leaf(&ca, &hosts(), now(), LEAF_DAYS).unwrap();
        let info = validate(&leaf.cert, &leaf.key, &ca.cert, now().unix_timestamp()).unwrap();
        assert_eq!(info.hosts(), hosts());
        assert!(info.not_after - info.not_before <= (LEAF_DAYS + 1) * 86_400);
        let other = generate_ca(&hosts(), now()).unwrap();
        let error =
            validate(&leaf.cert, &leaf.key, &other.cert, now().unix_timestamp()).unwrap_err();
        assert!(error.contains("does not verify"), "{error}");
    }

    #[test]
    fn a_leaf_outside_the_name_constraints_is_rejected() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        let outside = hosts_from(&[], &[], &["bank.example".to_string()]);
        let leaf = issue_leaf(&ca, &outside, now(), LEAF_DAYS).unwrap();
        let error = validate(&leaf.cert, &leaf.key, &ca.cert, now().unix_timestamp()).unwrap_err();
        assert!(error.contains("does not verify"), "{error}");
        let stray_ip = hosts_from(&[], &["10.9.9.9:1".to_string()], &[]);
        let leaf = issue_leaf(&ca, &stray_ip, now(), LEAF_DAYS).unwrap();
        assert!(validate(&leaf.cert, &leaf.key, &ca.cert, now().unix_timestamp()).is_err());
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        let one = issue_leaf(&ca, &hosts(), now(), LEAF_DAYS).unwrap();
        let two = issue_leaf(&ca, &hosts(), now(), LEAF_DAYS).unwrap();
        let error = validate(&one.cert, &two.key, &ca.cert, now().unix_timestamp()).unwrap_err();
        assert!(error.contains("does not belong"), "{error}");
        assert!(validate(&one.cert, "garbage", &ca.cert, now().unix_timestamp()).is_err());
    }

    #[test]
    fn expired_leaf_is_rejected() {
        let ca = generate_ca(&hosts(), now() - days(30)).unwrap();
        let leaf = issue_leaf(&ca, &hosts(), now() - days(20), 10).unwrap();
        assert!(validate(&leaf.cert, &leaf.key, &ca.cert, now().unix_timestamp()).is_err());
    }

    #[test]
    fn a_node_outside_the_sans_is_not_covered() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        let leaf = issue_leaf(&ca, &hosts(), now(), LEAF_DAYS).unwrap();
        let info = inspect(&leaf.cert).unwrap();
        assert!(covers(&info, "len-serv-001", &[]));
        assert!(covers(
            &info,
            "elsewhere",
            &["172.16.2.30".parse().unwrap()]
        ));
        assert!(!covers(
            &info,
            "unimatrixone",
            &["172.16.2.35".parse().unwrap()]
        ));
    }

    #[test]
    fn renewal_due_window() {
        let day = 86_400;
        assert!(!renew_due(100 * day, 0, RENEW_BEFORE_DAYS));
        assert!(renew_due(30 * day, 0, RENEW_BEFORE_DAYS));
        assert!(renew_due(-day, 0, RENEW_BEFORE_DAYS));
    }

    #[test]
    fn fingerprint_is_sha256_of_der() {
        let ca = generate_ca(&hosts(), now()).unwrap();
        let der = first_der(&ca.cert).unwrap();
        assert_eq!(
            inspect(&ca.cert).unwrap().fingerprint,
            hex::encode(Sha256::digest(&der))
        );
        assert_eq!(inspect(&ca.cert).unwrap().fingerprint.len(), 64);
    }

    #[test]
    fn oversized_pem_is_refused_before_parsing() {
        let huge = "x".repeat(MAX_PEM_BYTES + 1);
        assert!(
            validate(&huge, "k", "c", 0)
                .unwrap_err()
                .contains("limited")
        );
    }
}
