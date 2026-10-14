//! Self-signed TLS 1.3 identity for the agent-link server. The daemon pins the certificate's
//! SHA-256 fingerprint, which the agent publishes in its info file.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct TlsIdentity {
    pub config: Arc<rustls::ServerConfig>,
    /// Lower-case hex SHA-256 of the DER certificate.
    pub fingerprint: String,
}

impl TlsIdentity {
    pub fn generate() -> Result<Self, String> {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into(), "openprofiler-agent".into()])
            .map_err(|e| e.to_string())?;
        let cert = CertificateDer::from(ck.cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
        let fingerprint = hex::encode(Sha256::digest(cert.as_ref()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .map_err(|e| e.to_string())?;
        Ok(Self { config: Arc::new(config), fingerprint })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generates_identity() {
        let t = super::TlsIdentity::generate().unwrap();
        assert_eq!(t.fingerprint.len(), 64);
    }
}
