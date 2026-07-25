//! The router's local certificate authority (spec 0109).
//!
//! Interception — and *only* interception — needs to answer a TLS
//! handshake in the origin's name. That requires a CA the harness process
//! trusts. This one is:
//!
//! - **Per-installation, generated on first use**, stored under the
//!   daemon's state dir with owner-only permissions.
//! - **Stable across daemon restarts.** Adapters are reconnectable and
//!   outlive the daemon (`Adapter::spawn_reconnectable`), so a child that
//!   already loaded the CA path at spawn must keep validating against the
//!   same CA after the daemon comes back. Regenerating it would break
//!   live sessions.
//! - **Never installed into any system trust store.** It reaches exactly
//!   the processes Construct spawns, through a per-process environment
//!   variable, and nothing else on the machine trusts it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;

const CA_CERT_FILE: &str = "ca.pem";
const CA_KEY_FILE: &str = "ca-key.pem";
const CA_COMMON_NAME: &str = "Construct Router CA";

/// The CA's certificate parameters, rebuilt identically on every load.
///
/// This is what lets the CA be reloaded from a stored key without pulling
/// in an X.509 parser: [`Issuer`] needs the distinguished name, key-usage
/// set, and key-id method — not the encoded certificate — so reproducing
/// the same parameters reproduces the same issuer identity.
fn ca_params() -> CertificateParams {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_COMMON_NAME);
    dn.push(DnType::OrganizationName, "Construct");
    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

pub struct RouterCa {
    /// PEM path handed to harness processes through their CA-trust env var.
    cert_path: PathBuf,
    issuer: Issuer<'static, KeyPair>,
    /// Minted leaf certs, keyed by SNI host. Handshakes are per-connection
    /// and a session reconnects constantly; minting each time would burn
    /// measurable CPU on a hot path.
    leaves: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

impl RouterCa {
    /// Load the CA from `dir`, generating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create router state dir {}", dir.display()))?;
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);

        let key = if key_path.exists() && cert_path.exists() {
            let pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("read router CA key {}", key_path.display()))?;
            KeyPair::from_pem(&pem).context("parse router CA key")?
        } else {
            let key = KeyPair::generate().context("generate router CA key")?;
            let cert = ca_params()
                .self_signed(&key)
                .context("self-sign router CA")?;
            write_private(&key_path, &key.serialize_pem())?;
            std::fs::write(&cert_path, cert.pem())
                .with_context(|| format!("write router CA cert {}", cert_path.display()))?;
            key
        };

        Ok(Self {
            cert_path,
            issuer: Issuer::new(ca_params(), key),
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// Path to the CA certificate, for the harness's CA-trust env var.
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// A TLS server config presenting a freshly minted certificate for
    /// `host`.
    ///
    /// ALPN offers `http/1.1` only. Clients that would otherwise negotiate
    /// h2 with the real endpoint fall back, which keeps the intercepting
    /// path on one well-understood framing instead of two.
    pub fn server_config(&self, host: &str) -> Result<Arc<ServerConfig>> {
        if let Some(cached) = self.leaves.lock().unwrap().get(host) {
            return Ok(cached.clone());
        }
        let config = Arc::new(self.mint(host)?);
        self.leaves
            .lock()
            .unwrap()
            .insert(host.to_string(), config.clone());
        Ok(config)
    }

    fn mint(&self, host: &str) -> Result<ServerConfig> {
        let mut params = CertificateParams::new(vec![host.to_string()])
            .with_context(|| format!("leaf params for {host}"))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;

        let leaf_key = KeyPair::generate().context("generate leaf key")?;
        let leaf = params
            .signed_by(&leaf_key, &self.issuer)
            .with_context(|| format!("sign leaf for {host}"))?;

        let cert_der = CertificateDer::from(leaf.der().to_vec());
        let key_der = PrivatePkcs8KeyDer::from(leaf_key.serialize_der());

        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .context("router TLS protocol versions")?
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der.into())
            .context("router TLS server cert")?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
    }
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("write router CA key {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A regenerated CA would break every live session that already
    /// loaded the old one at spawn — adapters outlive the daemon.
    #[test]
    fn ca_is_stable_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = RouterCa::load_or_create(dir.path()).unwrap();
        let first_pem = std::fs::read_to_string(first.cert_path()).unwrap();
        drop(first);

        let second = RouterCa::load_or_create(dir.path()).unwrap();
        let second_pem = std::fs::read_to_string(second.cert_path()).unwrap();
        assert_eq!(first_pem, second_pem, "CA must survive a daemon restart");
    }

    #[test]
    fn mints_a_usable_leaf_for_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let ca = RouterCa::load_or_create(dir.path()).unwrap();
        let cfg = ca.server_config("api.anthropic.com").unwrap();
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);
        // Second call is served from cache — same Arc.
        let again = ca.server_config("api.anthropic.com").unwrap();
        assert!(Arc::ptr_eq(&cfg, &again));
    }

    #[cfg(unix)]
    #[test]
    fn ca_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        RouterCa::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(CA_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
