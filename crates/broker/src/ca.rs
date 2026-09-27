//! Per-session certificate authority.
//!
//! [`SessionCa::generate_with_sans`] builds a fresh CA and immediately signs a
//! single leaf certificate covering the session's broker virtual hosts. Only the
//! CA certificate (PEM) is exposed — via [`SessionCa::ca_cert_pem`] — so it can be
//! injected into the capsule trust store. Every private key (the CA key and the
//! leaf key) is wrapped in [`secrecy::SecretString`], never serialized, never
//! logged, and never returned across the crate boundary.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, GeneralSubtree, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use secrecy::{ExposeSecret, SecretString};

use capscode_core::BrokerError;

/// SANs baked into the broker leaf certificate. These are the per-provider
/// virtual hosts the AI tools are pointed at via `*_BASE_URL`.
pub(crate) const BROKER_SANS: [&str; 3] = [
    "anthropic.broker.capscode.local",
    "openai.broker.capscode.local",
    "moonshot.broker.capscode.local",
];

const BROKER_VHOST_SUFFIX: &str = ".broker.capscode.local";

fn broker_name_constraints(sans: &[String]) -> Option<NameConstraints> {
    let all_are_broker_vhosts = !sans.is_empty()
        && sans.iter().all(|san| {
            san.strip_suffix(BROKER_VHOST_SUFFIX)
                .is_some_and(|label| !label.is_empty() && !label.contains('.'))
        });
    all_are_broker_vhosts.then(|| NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(BROKER_VHOST_SUFFIX.to_string())],
        excluded_subtrees: Vec::new(),
    })
}

/// A per-session CA plus the broker leaf it signed.
///
/// The struct deliberately does **not** derive `Debug`: a manual, redacting impl
/// is provided so an accidental `{:?}` can never print key material.
pub struct SessionCa {
    /// The CA certificate in PEM form. This is the only thing that crosses into
    /// the capsule (the trust anchor); it contains no private key material.
    ca_cert_pem: String,
    /// The leaf certificate the broker presents when terminating TLS.
    leaf_cert_der: CertificateDer<'static>,
    /// The leaf private key, PEM-encoded and wrapped in secrecy.
    leaf_key_pem: SecretString,
}

impl SessionCa {
    /// Generates a fresh per-session CA and signs a broker leaf covering the
    /// three built-in provider virtual hosts ([`BROKER_SANS`]). Kept as the
    /// zero-argument entry point for callers that predate the data-driven
    /// providers manifest.
    pub fn generate() -> Result<SessionCa, BrokerError> {
        let default_sans: Vec<String> = BROKER_SANS.iter().map(|s| s.to_string()).collect();
        Self::generate_with_sans(&default_sans)
    }

    /// Generates a fresh per-session CA and signs a broker leaf whose SANs are the
    /// supplied virtual hosts — the vhosts of the selected providers manifest, so
    /// TLS termination succeeds for any user-selected provider (not just the three
    /// built-ins). An empty list falls back to [`BROKER_SANS`].
    pub fn generate_with_sans(sans: &[String]) -> Result<SessionCa, BrokerError> {
        let sans: Vec<String> = if sans.is_empty() {
            BROKER_SANS.iter().map(|s| s.to_string()).collect()
        } else {
            sans.to_vec()
        };
        // --- CA ---
        let ca_key =
            KeyPair::generate().map_err(|e| BrokerError::Tls(format!("ca keypair: {e}")))?;

        let mut ca_params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| BrokerError::Tls(format!("ca params: {e}")))?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.name_constraints = broker_name_constraints(&sans);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "CapsCode Session CA");

        let ca_cert = ca_params
            .self_signed(&ca_key)
            .map_err(|e| BrokerError::Tls(format!("ca self-sign: {e}")))?;
        let ca_cert_pem = ca_cert.pem();

        // --- leaf, signed by the CA ---
        let leaf_key =
            KeyPair::generate().map_err(|e| BrokerError::Tls(format!("leaf keypair: {e}")))?;
        let leaf_key_pem = SecretString::from(leaf_key.serialize_pem());

        let mut leaf_params = CertificateParams::new(sans)
            .map_err(|e| BrokerError::Tls(format!("leaf params: {e}")))?;
        leaf_params.is_ca = IsCa::NoCa;
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "capscode broker");

        // The issuer consumes the CA params + key; both are dropped (and the key
        // zeroized via the KeyPair drop) once the leaf is signed.
        let issuer: Issuer<'_, KeyPair> = Issuer::new(ca_params, ca_key);
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &issuer)
            .map_err(|e| BrokerError::Tls(format!("leaf sign: {e}")))?;
        let leaf_cert_der = leaf_cert.der().clone();

        Ok(SessionCa {
            ca_cert_pem,
            leaf_cert_der,
            leaf_key_pem,
        })
    }

    /// The CA certificate PEM — the only value injected into the capsule.
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// The certificate chain the broker presents on the capsule-facing side.
    /// Only the leaf is sent; the CA is the client's trust anchor, not a chain
    /// member.
    pub(crate) fn server_cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.leaf_cert_der.clone()]
    }

    /// The leaf private key in DER form, parsed on demand from the secret PEM.
    pub(crate) fn server_private_key(&self) -> Result<PrivateKeyDer<'static>, BrokerError> {
        let pem = self.leaf_key_pem.expose_secret();
        rustls_pemfile::private_key(&mut pem.as_bytes())
            .map_err(|e| BrokerError::Tls(format!("leaf key parse: {e}")))?
            .ok_or_else(|| BrokerError::Tls("leaf key: no PKCS#8 block found".to_string()))
    }
}

impl std::fmt::Debug for SessionCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render key material, and don't dump the whole cert either.
        f.debug_struct("SessionCa")
            .field("ca_cert_pem", &"<CA cert PEM>")
            .field("leaf_key_pem", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_session_ca_to_the_shared_broker_vhost_suffix() {
        let sans = vec![
            "anthropic.broker.capscode.local".to_string(),
            "codex-chatgpt.broker.capscode.local".to_string(),
        ];
        let constraints = broker_name_constraints(&sans).unwrap();
        assert_eq!(
            constraints.permitted_subtrees,
            vec![GeneralSubtree::DnsName(BROKER_VHOST_SUFFIX.to_string())]
        );
        assert!(constraints.excluded_subtrees.is_empty());
    }

    #[test]
    fn skips_session_ca_constraint_when_leaf_names_do_not_share_the_broker_suffix() {
        let sans = vec![
            "anthropic.broker.capscode.local".to_string(),
            "custom.example.test".to_string(),
        ];
        assert!(broker_name_constraints(&sans).is_none());
    }
}
