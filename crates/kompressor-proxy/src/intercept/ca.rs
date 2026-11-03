//! The local certificate authority, and the leaves it mints.
//!
//! This is the part of the listener that holds a signing key, so it is the
//! part worth being careful about. The key is written `0600`, it never leaves
//! the directory it was created in, and the fingerprint is logged on creation
//! so an operator can recognise their own CA in a trust store later — and
//! remove the right one when they are done with it.
//!
//! A leaf is minted per intercepted host and kept in memory only. Nothing
//! about a leaf is written down: it is cheap to mint again and a file of
//! minted leaves would be one more thing to leak.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::{Duration as TimeDuration, OffsetDateTime};

/// What went wrong reaching or using the authority.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("certificate authority at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("certificate authority at {path} is not usable: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("could not mint a certificate for {host}: {source}")]
    Mint {
        host: String,
        #[source]
        source: rcgen::Error,
    },
    #[error("rustls rejected a freshly minted certificate for {host}: {source}")]
    Rustls {
        host: String,
        #[source]
        source: rustls::Error,
    },
}

/// A CA that signs leaves on demand.
pub struct Authority {
    /// Signs leaves.
    ///
    /// rcgen 0.14 models the signing identity as an `Issuer` — a name, a key
    /// and the usages — rather than as a certificate. That removes the trap the
    /// field below was added for: there is no second certificate to accidentally
    /// present, because signing never produces one.
    issuer: Issuer<'static, KeyPair>,
    /// The certificate the operator actually trusted, byte for byte.
    ///
    /// Keeping this separate is the whole of the fix for a bug the reload test
    /// caught: rebuilding from PEM and calling `self_signed` again mints a new
    /// serial and signature, so the fingerprint moved between runs and the
    /// chain presented to clients was not the one in their trust store.
    /// Leaves still verify, because `signed_by` derives the issuer from the
    /// name and signs with the key, and both of those survive the round trip.
    der: CertificateDer<'static>,
    pem: String,
    path: PathBuf,
    alpn: Vec<Vec<u8>>,
    leaves: Mutex<LeafCache>,
}

/// Written by hand rather than derived: this struct holds a signing key, and
/// `#[derive(Debug)]` on it would put key material into any log line, panic
/// message or `unwrap_err` that ever formatted one.
impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("path", &self.path)
            .field("fingerprint", &self.fingerprint())
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Pick ring, once, for this process.
///
/// rustls 0.23 refuses to guess when several backends are compiled in, and
/// `reqwest` brings one of its own, so leaving it unset panics on the first
/// `ServerConfig::builder()` — in an accept loop, where a panic is a dropped
/// connection and no explanation. `install_default` losing a race is not an
/// error: it means another caller already chose, and any choice is fine.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Authority {
    /// Load the authority in `dir`, creating it if it is not there.
    ///
    /// Creating is the common path on a first run and must not be mistaken for
    /// an error; replacing one that exists must never happen, because the
    /// operator has by then installed it in a trust store and a silent
    /// regeneration would leave them trusting a key this process no longer
    /// has.
    pub fn load_or_create(
        dir: &Path,
        alpn: &[String],
        leaf_cache_entries: usize,
    ) -> Result<Self, CaError> {
        install_crypto_provider();

        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca.key");

        let (issuer, pem, der) = if cert_path.exists() || key_path.exists() {
            Self::load(dir, &cert_path, &key_path)?
        } else {
            Self::create(dir, &cert_path, &key_path)?
        };

        Ok(Authority {
            issuer,
            der,
            pem,
            path: dir.to_path_buf(),
            alpn: alpn.iter().map(|p| p.as_bytes().to_vec()).collect(),
            leaves: Mutex::new(LeafCache::new(leaf_cache_entries.max(1))),
        })
    }

    fn load(
        dir: &Path,
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<(Issuer<'static, KeyPair>, String, CertificateDer<'static>), CaError> {
        // Half a CA is not a CA. Saying which half is missing saves the
        // operator guessing whether to delete the directory.
        for p in [cert_path, key_path] {
            if !p.exists() {
                return Err(CaError::Invalid {
                    path: dir.to_path_buf(),
                    reason: format!(
                        "{} is missing; remove the directory to start again",
                        p.display()
                    ),
                });
            }
        }
        let cert_pem = std::fs::read_to_string(cert_path).map_err(|source| CaError::Io {
            path: cert_path.to_path_buf(),
            source,
        })?;
        let key_pem = std::fs::read_to_string(key_path).map_err(|source| CaError::Io {
            path: key_path.to_path_buf(),
            source,
        })?;
        let key = KeyPair::from_pem(&key_pem).map_err(|e| CaError::Invalid {
            path: key_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        // The issuer carries the name, the usages and the key. It is not a
        // certificate and cannot be mistaken for the one on disk.
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key).map_err(|e| CaError::Invalid {
            path: cert_path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let der = der_from_pem(cert_path, &cert_pem)?;
        Ok((issuer, cert_pem, der))
    }

    fn create(
        dir: &Path,
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<(Issuer<'static, KeyPair>, String, CertificateDer<'static>), CaError> {
        std::fs::create_dir_all(dir).map_err(|source| CaError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let key = KeyPair::generate().map_err(|e| CaError::Invalid {
            path: dir.to_path_buf(),
            reason: e.to_string(),
        })?;

        let mut name = DistinguishedName::new();
        // Named for what it is, so it is recognisable in a trust store beside
        // the real authorities it will be sitting next to.
        name.push(DnType::CommonName, "OptScale Kompressor local CA");
        name.push(DnType::OrganizationName, "OptScale Kompressor");

        let (not_before, not_after) = validity(CA_DAYS);
        let mut params = CertificateParams::default();
        params.distinguished_name = name;
        params.not_before = not_before;
        params.not_after = not_after;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];

        let certificate = params.self_signed(&key).map_err(|e| CaError::Invalid {
            path: dir.to_path_buf(),
            reason: e.to_string(),
        })?;
        let pem = certificate.pem();

        write_private(key_path, key.serialize_pem().as_bytes())?;
        std::fs::write(cert_path, pem.as_bytes()).map_err(|source| CaError::Io {
            path: cert_path.to_path_buf(),
            source,
        })?;

        tracing::info!(
            path = %cert_path.display(),
            fingerprint = %fingerprint(certificate.der()),
            "created a local certificate authority; trust it to intercept, and remove it when done"
        );

        let der = certificate.der().clone();
        // Params and key become the issuer; the certificate's bytes are what we
        // present and are kept separately.
        Ok((Issuer::new(params, key), pem, der))
    }

    /// Where the certificate an operator has to trust can be found.
    pub fn certificate_path(&self) -> PathBuf {
        self.path.join("ca.pem")
    }

    /// The CA certificate in PEM, for callers that would rather print it than
    /// read the file.
    pub fn certificate_pem(&self) -> &str {
        &self.pem
    }

    /// SHA-256 over the DER, in the colon-separated form trust stores show.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.der)
    }

    /// A `ServerConfig` presenting a leaf for `host`, minting one if this is
    /// the first time that host has been asked for.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>, CaError> {
        let host = host.to_ascii_lowercase();
        if let Some(found) = self.leaves.lock().expect("leaf cache mutex").get(&host) {
            return Ok(found);
        }
        let built = self.mint(&host)?;
        self.leaves
            .lock()
            .expect("leaf cache mutex")
            .insert(host, Arc::clone(&built));
        Ok(built)
    }

    fn mint(&self, host: &str) -> Result<Arc<ServerConfig>, CaError> {
        let key = KeyPair::generate().map_err(|source| CaError::Mint {
            host: host.to_string(),
            source,
        })?;

        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, host);

        let (not_before, not_after) = validity(LEAF_DAYS);
        let mut params = CertificateParams::default();
        params.distinguished_name = name;
        params.not_before = not_before;
        params.not_after = not_after;
        // A leaf that could sign would turn a stolen leaf into a second CA.
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![san_for(host)];

        let leaf = params
            .signed_by(&key, &self.issuer)
            .map_err(|source| CaError::Mint {
                host: host.to_string(),
                source,
            })?;

        let chain: Vec<CertificateDer<'static>> = vec![leaf.der().clone(), self.der.clone()];
        let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, private)
            .map_err(|source| CaError::Rustls {
                host: host.to_string(),
                source,
            })?;
        config.alpn_protocols = self.alpn.clone();
        Ok(Arc::new(config))
    }
}

/// How long a generated certificate is good for.
///
/// rcgen's default window is 1975 to 4096 — a 2121-year certificate, which is
/// not something any real authority issues. Trust stores and clients are
/// entitled to be suspicious of it, and an operator reading the dates in their
/// own trust store should see something that looks deliberate.
///
/// The CA gets three years, matching what mitmproxy issues for the same job.
/// A leaf gets ninety days: it is minted on demand and cached in memory only,
/// so nothing depends on it outliving the process.
fn validity(days: i64) -> (OffsetDateTime, OffsetDateTime) {
    let now = OffsetDateTime::now_utc();
    // Backdated slightly so a client whose clock is a little behind does not
    // reject a certificate issued seconds ago.
    (now - TimeDuration::hours(1), now + TimeDuration::days(days))
}

const CA_DAYS: i64 = 365 * 3;
const LEAF_DAYS: i64 = 90;

/// An IP literal belongs in an IP SAN; a name belongs in a DNS SAN. A client
/// checking an address against a DNS entry fails the handshake.
fn san_for(host: &str) -> SanType {
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => SanType::IpAddress(ip),
        Err(_) => SanType::DnsName(host.to_string().try_into().unwrap_or_else(|_| {
            // An unparseable name cannot produce a usable leaf either way; a
            // placeholder keeps this infallible and the handshake fails
            // honestly instead of panicking in an accept loop.
            "invalid.invalid".to_string().try_into().expect("literal")
        })),
    }
}

/// The DER inside a PEM file, so the bytes presented are the bytes on disk.
fn der_from_pem(path: &Path, body: &str) -> Result<CertificateDer<'static>, CaError> {
    let parsed = pem::parse(body).map_err(|e| CaError::Invalid {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if parsed.tag() != "CERTIFICATE" {
        return Err(CaError::Invalid {
            path: path.to_path_buf(),
            reason: format!("expected a CERTIFICATE block, found {}", parsed.tag()),
        });
    }
    Ok(CertificateDer::from(parsed.into_contents()))
}

fn fingerprint(der: &CertificateDer<'_>) -> String {
    use std::fmt::Write as _;
    let digest = ring_sha256(der.as_ref());
    let mut out = String::with_capacity(digest.len() * 3);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{byte:02X}");
    }
    out
}

fn ring_sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

/// Write a key so only its owner can read it.
///
/// The mode is set before the bytes land, not after: a key that is briefly
/// world-readable is a key that was world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CaError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes).map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Bounded, insertion-ordered, and small. A handful of hosts are ever
/// intercepted, so eviction is a safety valve rather than a hot path.
struct LeafCache {
    limit: usize,
    order: Vec<String>,
    entries: HashMap<String, Arc<ServerConfig>>,
}

impl LeafCache {
    fn new(limit: usize) -> Self {
        LeafCache {
            limit,
            order: Vec::new(),
            entries: HashMap::new(),
        }
    }

    fn get(&self, host: &str) -> Option<Arc<ServerConfig>> {
        self.entries.get(host).map(Arc::clone)
    }

    fn insert(&mut self, host: String, config: Arc<ServerConfig>) {
        if self.entries.contains_key(&host) {
            return;
        }
        if self.order.len() >= self.limit {
            let evicted = self.order.remove(0);
            self.entries.remove(&evicted);
        }
        self.order.push(host.clone());
        self.entries.insert(host, config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kompressor-ca-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_once_and_reloads_the_same_authority() {
        let dir = temp_dir("reload");
        let first = Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap();
        let before = first.fingerprint();
        drop(first);

        // A second start must not mint a second CA: the operator has already
        // trusted the first one.
        let second = Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap();
        assert_eq!(before, second.fingerprint());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("mode");
        let ca = Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap();
        let mode = std::fs::metadata(dir.join("ca.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} has mode {mode:o}",
            ca.certificate_path().display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mints_a_leaf_and_serves_the_same_one_again() {
        let dir = temp_dir("leaf");
        let ca = Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap();
        let a = ca.server_config("api.anthropic.com").unwrap();
        let b = ca.server_config("API.anthropic.com").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "case-folded host missed the cache");
        assert_eq!(a.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn certificates_have_a_believable_validity_window() {
        // rcgen's default is 1975 to 4096. A certificate valid for twenty-one
        // centuries is not one any trust store should be asked to accept.
        // Asserted on the window itself rather than on a built certificate:
        // rcgen 0.14 models the signing identity as an `Issuer`, which exposes
        // no parameters to read back. The window is what both the CA and the
        // leaf paths set, so it is the thing worth pinning — and a real
        // certificate carrying it is covered end to end by the handshake tests.
        let now = OffsetDateTime::now_utc();

        let (ca_before, ca_after) = validity(CA_DAYS);
        assert!(ca_before <= now, "a backdated start, for slow clocks");
        let ca_years = (ca_after - now).whole_days() / 365;
        assert!((2..=4).contains(&ca_years), "CA valid for {ca_years} years");

        let (leaf_before, leaf_after) = validity(LEAF_DAYS);
        assert!(leaf_before <= now);
        let leaf_days = (leaf_after - now).whole_days();
        assert!(
            (80..=100).contains(&leaf_days),
            "leaf valid for {leaf_days} days"
        );
    }

    #[test]
    fn half_an_authority_is_refused_rather_than_replaced() {
        let dir = temp_dir("half");
        Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap();
        std::fs::remove_file(dir.join("ca.key")).unwrap();
        let err = Authority::load_or_create(&dir, &["http/1.1".into()], 8).unwrap_err();
        assert!(matches!(err, CaError::Invalid { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_evicts_rather_than_growing_without_bound() {
        let dir = temp_dir("evict");
        let ca = Authority::load_or_create(&dir, &["http/1.1".into()], 2).unwrap();
        let first = ca.server_config("one.test").unwrap();
        ca.server_config("two.test").unwrap();
        ca.server_config("three.test").unwrap();
        let again = ca.server_config("one.test").unwrap();
        assert!(!Arc::ptr_eq(&first, &again), "nothing was evicted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
