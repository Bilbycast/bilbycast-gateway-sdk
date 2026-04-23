// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

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
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            roots: root_store,
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
#[derive(Debug)]
struct PinnedCertVerifier {
    roots: rustls::RootCertStore,
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
        let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(self.roots.clone()))
            .build()
            .map_err(|e| RustlsError::General(format!("failed to build webpki verifier: {e}")))?;
        verifier.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;

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

    #[test]
    fn fingerprint_hex_colons_matches_known_value() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let fp = fingerprint_hex_colons(&[]);
        assert!(fp.starts_with("e3:b0:c4:42"));
    }
}
