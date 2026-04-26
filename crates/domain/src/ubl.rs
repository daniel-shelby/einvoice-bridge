//! UBL invoice construction and the LHDN two-stage signing flow.
//!
//! Step 5a scope: minimal-but-real builder. The pipeline (canonicalise →
//! hash → sign → embed → re-canonicalise → final hash) is correct; the
//! invoice fields themselves are intentionally sparse — only enough top-
//! level structure that the LHDN-side shape is recognisable. Filling in
//! parties, lines, totals, tax subtotals, and the full XAdES property
//! tree to LHDN-acceptance level is step 5b, ideally iterating against
//! a real preprod sandbox.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::DomainError;
use crate::canonicalize::canonicalize_json_bytes;
use crate::digest::sha256_b64;
use crate::signer::Signer;

/// Output of `build_signed_document`. All fields are exactly what the
/// downstream pieces need.
#[derive(Debug, Clone)]
pub struct SignedDocument {
    /// Final signed UBL JSON, canonicalised. Base64-encode this and put
    /// it in LHDN's `document` field.
    pub canonical_bytes: Vec<u8>,
    /// `base64(SHA-256(canonical_bytes))` — goes in LHDN's `documentHash`
    /// AND in `invoices.doc_digest`.
    pub document_hash: String,
    /// POS-side invoice number, copied through. Goes in LHDN's
    /// `codeNumber`.
    pub code_number: String,
    /// `base64(RSA-SHA256(signed_payload))` — stored in
    /// `invoices.signature` for ops/debugging.
    pub signature: String,
    /// The exact bytes the signature is computed over (the unsigned
    /// canonical UBL doc). Exposed so tests can verify the signature
    /// round-trips with the signer's public key, and so an operator can
    /// reproduce the digest if there's ever a dispute.
    pub signed_payload: Vec<u8>,
}

/// Build, canonicalise, sign, and re-canonicalise a UBL invoice from a
/// raw POS payload.
pub fn build_signed_document(
    pos_payload: &Value,
    signer: &Signer,
    signing_time: OffsetDateTime,
) -> Result<SignedDocument, DomainError> {
    let code_number = pos_payload
        .get("invoice_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DomainError::InvalidInvoice("missing invoice_ref".into()))?
        .to_owned();

    // 1. Build the unsigned UBL doc.
    let unsigned = build_unsigned_invoice(pos_payload, &code_number);

    // 2. Canonicalise the unsigned doc — these are the bytes the
    //    signature is computed over (LHDN: "Sig over DocDigest").
    let signed_payload = canonicalize_json_bytes(&unsigned)?;

    // 3. Sign. RSA-SHA256 PKCS#1 v1.5; deterministic.
    let signature = B64.encode(signer.sign(&signed_payload));

    // 4. Cert metadata for the signed-properties block.
    let cert_der = signer.certificate_der();
    let cert_digest = sha256_b64(cert_der);
    let cert_b64 = B64.encode(cert_der);
    let signing_time_str = signing_time
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|e| DomainError::InvalidInvoice(format!("signing time: {e}")))?;

    // 5. Embed signature + extensions.
    let signed = embed_signature(
        unsigned,
        &signature,
        &cert_digest,
        &cert_b64,
        &signing_time_str,
    )?;

    // 6. Re-canonicalise. These bytes are what LHDN actually sees.
    let canonical_bytes = canonicalize_json_bytes(&signed)?;
    let document_hash = sha256_b64(&canonical_bytes);

    Ok(SignedDocument {
        canonical_bytes,
        document_hash,
        code_number,
        signature,
        signed_payload,
    })
}

/// Construct the unsigned UBL JSON document for an invoice.
///
/// 5a scope: top-level skeleton only. The party/line/total subtrees are
/// intentionally absent — 5b will fill them in once we can validate
/// against the real LHDN sandbox.
fn build_unsigned_invoice(pos: &Value, code_number: &str) -> Value {
    let issue_date = pos.get("issue_date").and_then(Value::as_str).unwrap_or("");
    let issue_time = pos.get("issue_time").and_then(Value::as_str).unwrap_or("");
    let currency = pos.get("currency").and_then(Value::as_str).unwrap_or("MYR");

    json!({
        "_D": "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2",
        "_A": "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2",
        "_B": "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2",
        "Invoice": [{
            "ID":                   [{"_": code_number}],
            "IssueDate":            [{"_": issue_date}],
            "IssueTime":            [{"_": issue_time}],
            "InvoiceTypeCode":      [{"_": "01", "listVersionID": "1.0"}],
            "DocumentCurrencyCode": [{"_": currency}],
            "TaxCurrencyCode":      [{"_": currency}],
        }]
    })
}

/// Add the `UBLExtensions` (carrying the digital signature block) and
/// the top-level `Signature` reference back onto the invoice.
fn embed_signature(
    mut doc: Value,
    signature_b64: &str,
    cert_digest_b64: &str,
    cert_b64: &str,
    signing_time: &str,
) -> Result<Value, DomainError> {
    let extensions = json!([{
        "UBLExtension": [{
            "ExtensionURI":     [{"_": "urn:oasis:names:specification:ubl:dsig:enveloped:xades"}],
            "ExtensionContent": [{
                "UBLDocumentSignatures": [{
                    "SignatureInformation": [{
                        "ID":         [{"_": "signature1"}],
                        "Signature":  [{
                            "SignatureValue": [{"_": signature_b64}],
                            "KeyInfo": [{
                                "X509Data": [{
                                    "X509Certificate": [{"_": cert_b64}]
                                }]
                            }],
                            "Object": [{
                                "QualifyingProperties": [{
                                    "SignedProperties": [{
                                        "SignedSignatureProperties": [{
                                            "SigningTime": [{"_": signing_time}],
                                            "SigningCertificate": [{
                                                "Cert": [{
                                                    "CertDigest": [{
                                                        "DigestValue":  [{"_": cert_digest_b64}],
                                                        "DigestMethod": [{
                                                            "_": "",
                                                            "Algorithm": "http://www.w3.org/2001/04/xmlenc#sha256"
                                                        }]
                                                    }]
                                                }]
                                            }]
                                        }]
                                    }]
                                }]
                            }]
                        }]
                    }]
                }]
            }]
        }]
    }]);

    let signature_ref = json!([{ "ID": [{"_": "signature1"}] }]);

    let invoice_arr = doc
        .get_mut("Invoice")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| DomainError::InvalidInvoice("missing Invoice array".into()))?;
    let invoice = invoice_arr
        .get_mut(0)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| DomainError::InvalidInvoice("empty Invoice array".into()))?;
    invoice.insert("UBLExtensions".into(), extensions);
    invoice.insert("Signature".into(), signature_ref);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPrivateKey;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use serde_json::json;
    use sha2::Sha256;
    use time::macros::datetime;

    fn ephemeral_signer() -> (Signer, RsaPrivateKey) {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 1024).expect("generate key");
        let signer = Signer::from_parts(key.clone(), b"placeholder-cert-der".to_vec());
        (signer, key)
    }

    fn sample_payload() -> Value {
        json!({
            "invoice_ref": "INV-T1",
            "issue_date": "2026-04-26",
            "issue_time": "14:30:00",
            "currency": "MYR"
        })
    }

    #[test]
    fn build_signed_document_is_deterministic_for_fixed_inputs() {
        // PKCS#1 v1.5 is deterministic, so given the same payload + signer
        // + signing_time the output bytes must match exactly.
        let (signer, _) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);

        let a = build_signed_document(&sample_payload(), &signer, t).unwrap();
        let b = build_signed_document(&sample_payload(), &signer, t).unwrap();

        assert_eq!(a.canonical_bytes, b.canonical_bytes);
        assert_eq!(a.document_hash, b.document_hash);
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn document_hash_is_sha256_of_canonical_bytes() {
        let (signer, _) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);
        let signed = build_signed_document(&sample_payload(), &signer, t).unwrap();

        let recomputed = sha256_b64(&signed.canonical_bytes);
        assert_eq!(recomputed, signed.document_hash);
    }

    #[test]
    fn signature_verifies_with_signers_public_key() {
        let (signer, key) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);
        let signed = build_signed_document(&sample_payload(), &signer, t).unwrap();

        let sig_bytes = B64.decode(&signed.signature).unwrap();
        let sig = Signature::try_from(sig_bytes.as_slice()).unwrap();
        let vkey = VerifyingKey::<Sha256>::new(key.to_public_key());
        vkey.verify(&signed.signed_payload, &sig)
            .expect("signature must verify against signer's public key");
    }

    #[test]
    fn code_number_propagates_from_invoice_ref() {
        let (signer, _) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);
        let signed = build_signed_document(&sample_payload(), &signer, t).unwrap();
        assert_eq!(signed.code_number, "INV-T1");
    }

    #[test]
    fn missing_invoice_ref_is_a_domain_error() {
        let (signer, _) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);
        let payload = json!({ "issue_date": "2026-04-26" });
        let err = build_signed_document(&payload, &signer, t).unwrap_err();
        assert!(matches!(err, DomainError::InvalidInvoice(_)), "got {err:?}");
    }

    #[test]
    fn signed_doc_contains_ubl_extensions_and_signature_keys() {
        let (signer, _) = ephemeral_signer();
        let t = datetime!(2026-04-26 14:30:00 UTC);
        let signed = build_signed_document(&sample_payload(), &signer, t).unwrap();

        let parsed: Value = serde_json::from_slice(&signed.canonical_bytes).unwrap();
        let invoice = &parsed["Invoice"][0];
        assert!(
            invoice.get("UBLExtensions").is_some(),
            "missing UBLExtensions"
        );
        assert!(invoice.get("Signature").is_some(), "missing Signature");
        assert_eq!(invoice["ID"][0]["_"], "INV-T1");
    }
}
