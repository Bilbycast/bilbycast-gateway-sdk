// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! TLS configuration for the manager WebSocket connection.
//!
//! Three modes, selected by `GatewayConfig`:
//! - **Standard**: validate against system CA roots (`webpki-roots`). Default.
//! - **Self-signed**: accept any certificate. Requires `BILBYCAST_ALLOW_INSECURE=1`
//!   in the environment as a safety guard; otherwise returns an error.
//! - **Pinned**: validate chain normally AND require the leaf cert's SHA-256
//!   fingerprint to match a configured value.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::errors::SdkError;

/// Build a `rustls::ClientConfig` honouring the three TLS modes.
pub fn build_tls_config(
    accept_self_signed: bool,
    cert_fingerprint: Option<&str>,
) -> Result<ClientConfig, SdkError> {
    // Ensure the default crypto provider is installed. Idempotent.
    // tokio-tungstenite's `rustls-tls-webpki-roots` feature pulls this in,
    // but if the consumer depends on a different rustls feature combination
    // we make sure initialization happens exactly once.
    install_default_crypto_provider_once();

    if let Some(fp) = cert_fingerprint {
        let fp = fp.trim();
        if !fp.is_empty() {
            return build_pinned_config(fp);
        }
    }

    if accept_self_signed {
        return build_insecure_config();
    }

    build_standard_config()
}

fn install_default_crypto_provider_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Prefer aws-lc-rs if present (default with `rustls` crate's default
        // features), fall back to ring if that's what's installed. In our
        // Cargo.toml we rely on tokio-tungstenite's rustls features — the
        // provider will already be wired. This is a no-op in most builds.
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );
    });
}

fn build_standard_config() -> Result<ClientConfig, SdkError> {
    let root_store = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(config)
}

fn build_insecure_config() -> Result<ClientConfig, SdkError> {
    let allow = std::env::var("BILBYCAST_ALLOW_INSECURE")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !allow {
        return Err(SdkError::Tls(
            "accept_self_signed_cert is enabled but BILBYCAST_ALLOW_INSECURE=1 is not set. \
             Set this environment variable to confirm you understand the security implications."
                .into(),
        ));
    }

    tracing::warn!(
        "SECURITY WARNING: accept_self_signed_cert is enabled — ALL TLS certificate \
         validation is disabled for the manager connection. This is vulnerable to \
         man-in-the-middle attacks. Do NOT use this in production."
    );

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    Ok(config)
}

fn build_pinned_config(fingerprint: &str) -> Result<ClientConfig, SdkError> {
    let fp = normalise_fingerprint(fingerprint)?;

    tracing::info!(
        "Certificate pinning enabled (fingerprint prefix: {}...)",
        &fp[..fp.len().min(20)]
    );

    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // Build the standard webpki verifier ONCE, here, and keep it: the pinned
    // verifier delegates chain validation *and* the CertificateVerify
    // signature checks to it. Building it inside `verify_server_cert` (as an
    // earlier version did) leaves the signature callbacks with nothing to
    // delegate to, which is how they came to assert validity unchecked.
    let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| SdkError::Tls(format!("failed to build certificate verifier: {e}")))?;
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            inner,
            expected_fingerprint: fp,
        }))
        .with_no_client_auth();
    Ok(config)
}

/// Accept either `ab:cd:ef:…` or `abcdef…` (case-insensitive) — normalise
/// to lowercase colon-separated hex so comparisons are stable.
pub fn normalise_fingerprint(input: &str) -> Result<String, SdkError> {
    let clean: String = input.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect();
    if clean.len() != 64 {
        return Err(SdkError::Tls(format!(
            "cert_fingerprint must be 32 bytes (64 hex chars, optional `:` separators); got {} hex chars",
            clean.len()
        )));
    }
    if !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(SdkError::Tls(
            "cert_fingerprint contains non-hex characters".into(),
        ));
    }
    let lower = clean.to_ascii_lowercase();
    let mut out = String::with_capacity(64 + 31);
    for (i, byte) in lower.as_bytes().chunks(2).enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push(byte[0] as char);
        out.push(byte[1] as char);
    }
    Ok(out)
}

/// Verifier that accepts any certificate (used for dev/testing with self-signed).
#[derive(Debug)]
struct InsecureCertVerifier;

impl ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ALL_SCHEMES.to_vec()
    }
}

/// Verifier that validates chain normally AND pins to a SHA-256 fingerprint.
///
/// Every check other than the fingerprint pin is delegated to `inner`, the
/// standard webpki verifier — including the two `CertificateVerify`
/// signature callbacks. That delegation is load-bearing: certificates are
/// public, so an on-path attacker can replay the real manager's leaf (chain
/// validates, fingerprint matches) and only the CertificateVerify signature
/// proves possession of the matching private key.
#[derive(Debug)]
struct PinnedCertVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    expected_fingerprint: String,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // 1. Chain validation against webpki roots. Pinning ADDS to, not
        //    replaces, standard validation — otherwise a correct fingerprint
        //    would bypass expiry and name checks.
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;

        // 2. Fingerprint check.
        let fp = fingerprint_hex_colons(end_entity.as_ref());
        if fp == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(format!(
                "certificate fingerprint mismatch — expected {}, got {}. \
                 Possible MITM or certificate rotation.",
                self.expected_fingerprint, fp
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

const ALL_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PKCS1_SHA256,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::ECDSA_NISTP256_SHA256,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ECDSA_NISTP521_SHA512,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::ED25519,
];

/// Compute SHA-256 over the DER-encoded certificate and return `aa:bb:cc:…`.
pub fn fingerprint_hex_colons(der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(der);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64 + 31);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_normalisation_accepts_either_form() {
        let colons = "ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89";
        let plain = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(normalise_fingerprint(colons).unwrap(), colons);
        assert_eq!(normalise_fingerprint(plain).unwrap(), colons);
    }

    #[test]
    fn fingerprint_normalisation_rejects_bad_length() {
        assert!(normalise_fingerprint("deadbeef").is_err());
    }

    /// A P-256 leaf certificate (same fixture the upgrade chain tests use).
    /// Only its SPKI matters here — we never chain-validate it.
    const LEAF_PEM: &str = concat!(
        "MIIBbDCCARKgAwIBAgIUack56LkOqBV3bH7TCvFYEhf+oHIwCgYIKoZIzj0EAwIw",
        "GTEXMBUGA1UEAwwOVGVzdCBGdWxjaW8gQ0EwHhcNMjYwNzA2MDI1ODMxWhcNMjcw",
        "NzA2MDI1ODMxWjAPMQ0wCwYDVQQDDARsZWFmMFkwEwYHKoZIzj0CAQYIKoZIzj0D",
        "AQcDQgAEAXclNg9sf5hNuqiqxe4voL5sXVi8pccpprcDpueQIvp4j4KHv5wza7LT",
        "ykGo4FRVpx6Zggk0rN/Sca7siAFkv6NCMEAwHQYDVR0OBBYEFNCUvLZDwM0sxQYH",
        "toVAKni9r/t+MB8GA1UdIwQYMBaAFHtikya5y+JL+so1++HmdCdh5Qu8MAoGCCqG",
        "SM49BAMCA0gAMEUCIGoE4eNGJB2BSBR6+IFNlakoKcEPB66LyXOnGWX/EDonAiEA",
        "zXkAMcVKpLiuTmuWzC/vuzBmQpdkQ6UW+fVqx7oOkQQ=",
    );

    fn test_pinned_verifier() -> PinnedCertVerifier {
        install_default_crypto_provider_once();
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("build webpki verifier");
        PinnedCertVerifier {
            inner,
            expected_fingerprint: normalise_fingerprint(&"ab".repeat(32)).unwrap(),
        }
    }

    /// Wire-encode a `DigitallySignedStruct` (u16 scheme, u16 length, sig).
    fn dss(scheme: SignatureScheme, sig: &[u8]) -> DigitallySignedStruct {
        use rustls::internal::msgs::codec::{Codec, Reader};
        let mut wire = Vec::new();
        scheme.encode(&mut wire);
        wire.extend_from_slice(&(sig.len() as u16).to_be_bytes());
        wire.extend_from_slice(sig);
        DigitallySignedStruct::read(&mut Reader::init(&wire)).expect("decode dss")
    }

    /// Regression test for the CertificateVerify bypass: both signature
    /// callbacks used to return `assertion()` unconditionally, so an on-path
    /// attacker replaying the manager's (public) certificate completed the
    /// handshake without ever holding the private key. A forged signature
    /// must now be rejected.
    #[test]
    fn pinned_verifier_rejects_forged_handshake_signature() {
        use base64::Engine;
        let der = base64::engine::general_purpose::STANDARD
            .decode(LEAF_PEM)
            .expect("fixture is base64");
        let cert = CertificateDer::from(der);
        let verifier = test_pinned_verifier();
        let bogus = dss(SignatureScheme::ECDSA_NISTP256_SHA256, &[0u8; 64]);

        assert!(
            verifier
                .verify_tls13_signature(b"transcript", &cert, &bogus)
                .is_err(),
            "TLS 1.3 CertificateVerify must be checked, not asserted"
        );
        assert!(
            verifier
                .verify_tls12_signature(b"transcript", &cert, &bogus)
                .is_err(),
            "TLS 1.2 CertificateVerify must be checked, not asserted"
        );
    }

    /// The pinned verifier must advertise the schemes its inner verifier can
    /// actually check — advertising a scheme it cannot verify would make the
    /// server pick one and then fail every handshake.
    #[test]
    fn pinned_verifier_schemes_come_from_inner() {
        let verifier = test_pinned_verifier();
        assert_eq!(
            verifier.supported_verify_schemes(),
            verifier.inner.supported_verify_schemes()
        );
    }

    #[test]
    fn fingerprint_hex_colons_matches_known_value() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let fp = fingerprint_hex_colons(&[]);
        assert!(fp.starts_with("e3:b0:c4:42"));
    }
}
