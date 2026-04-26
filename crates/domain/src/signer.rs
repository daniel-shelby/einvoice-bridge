//! P12 loading + RSA-SHA256 (PKCS#1 v1.5) signer.
//!
//! Holds the parsed private key and the leaf certificate's DER bytes. The
//! cert bytes are exposed because LHDN's signed-properties block embeds
//! both the certificate and a digest of it.

use crate::DomainError;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer as _};
use sha2::Sha256;

pub struct Signer {
    private_key: RsaPrivateKey,
    cert_der: Vec<u8>,
}

impl Signer {
    /// Load a PKCS#12 (`.p12` / `.pfx`) bundle, decrypt with `password`,
    /// and extract the first private key + certificate.
    pub fn from_p12(pkcs12_bytes: &[u8], password: &str) -> Result<Self, DomainError> {
        let pfx = p12::PFX::parse(pkcs12_bytes)
            .map_err(|e| DomainError::Sign(format!("parse pkcs12: {e}")))?;

        let mut key_bags = pfx
            .key_bags(password)
            .map_err(|e| DomainError::Sign(format!("decrypt pkcs12 keys: {e}")))?;
        let mut cert_bags = pfx
            .cert_bags(password)
            .map_err(|e| DomainError::Sign(format!("decrypt pkcs12 certs: {e}")))?;

        // TODO: when we have a real LHDN preprod cert, match by friendly-name
        // (PKCS#12 attribute `friendlyName` / `localKeyId`) instead of taking the
        // last bag. This works for single-key/single-cert .p12 files but will
        // grab the wrong entry if the bundle includes intermediates.
        let key_der = key_bags
            .pop()
            .ok_or_else(|| DomainError::Sign("no private key in pkcs12".into()))?;
        let cert_der = cert_bags
            .pop()
            .ok_or_else(|| DomainError::Sign("no certificate in pkcs12".into()))?;

        let private_key = RsaPrivateKey::from_pkcs8_der(&key_der)
            .map_err(|e| DomainError::Sign(format!("parse private key: {e}")))?;

        Ok(Self {
            private_key,
            cert_der,
        })
    }

    /// Construct from already-parsed parts. Useful for tests so we don't
    /// have to bake a real `.p12` into the repo.
    pub fn from_parts(private_key: RsaPrivateKey, cert_der: Vec<u8>) -> Self {
        Self {
            private_key,
            cert_der,
        }
    }

    /// Sign `message` with RSA-SHA256 (PKCS#1 v1.5). Hashing happens
    /// internally — pass the raw bytes you want signed.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let signing_key = SigningKey::<Sha256>::new(self.private_key.clone());
        signing_key.sign(message).to_bytes().to_vec()
    }

    /// DER-encoded leaf certificate. Needed for the UBL `X509Certificate`
    /// element and for the `CertDigest`.
    pub fn certificate_der(&self) -> &[u8] {
        &self.cert_der
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;

    fn ephemeral_signer() -> Signer {
        let mut rng = rand::thread_rng();
        // 1024 bits keeps tests fast — for production we always use the
        // bits that ship in the LHDN-issued certificate.
        let key = RsaPrivateKey::new(&mut rng, 1024).expect("generate key");
        Signer::from_parts(key, b"placeholder-cert-der".to_vec())
    }

    #[test]
    fn round_trip_signature_verifies() {
        let signer = ephemeral_signer();
        let msg = b"hello LHDN";
        let sig_bytes = signer.sign(msg);

        let verifying_key = VerifyingKey::<Sha256>::new(signer.private_key.to_public_key());
        let sig = Signature::try_from(sig_bytes.as_slice()).expect("decode sig");
        verifying_key.verify(msg, &sig).expect("must verify");
    }

    #[test]
    fn pkcs1v15_signatures_are_deterministic() {
        // PKCS#1 v1.5 is deterministic — same input must produce the same
        // bytes, byte for byte.
        let signer = ephemeral_signer();
        assert_eq!(signer.sign(b"x"), signer.sign(b"x"));
    }

    #[test]
    fn signature_changes_when_message_changes() {
        let signer = ephemeral_signer();
        assert_ne!(signer.sign(b"a"), signer.sign(b"b"));
    }

    #[test]
    fn certificate_der_is_exposed() {
        let signer = ephemeral_signer();
        assert_eq!(signer.certificate_der(), b"placeholder-cert-der");
    }

    #[test]
    fn from_p12_rejects_garbage() {
        let result = Signer::from_p12(b"this is not a pkcs12 file", "anything");
        assert!(result.is_err(), "garbage input must error, not panic");
    }
}
