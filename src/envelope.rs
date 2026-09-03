//! Signed settings envelopes: betamacs's re-implementation of otactl's
//! firmware-manifest verification (docs/managed-mode.md).
//!
//! The contract is otactl's `internal/signer` + `internal/firmware`:
//! the signed bytes are the manifest rendered exactly like Go's
//! `json.MarshalIndent(m, "", "  ")` plus a trailing newline (struct
//! field order, omitempty semantics, HTML-safe escaping), signed
//! ECDSA P-256/SHA-256 (ASN.1 DER signature, std base64), with a signer
//! certificate chaining to the otactl root pinned in the app bundle —
//! anchors only, leaf must carry the code-signing EKU. The artifact is
//! bound by the manifest's sha256; the global `epoch` is monotonic.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use base64::Engine;
use p256::ecdsa::signature::Verifier as _;
use sha2::Digest;
use x509_cert::der::asn1::ObjectIdentifier;
use x509_cert::der::{Decode, Encode};
use x509_cert::Certificate;

/// The otactl app names envelopes may carry: settings and the .app zip.
pub const CONFIG_APP: &str = "betamacs-config";
#[allow(dead_code)] // used by betamacsd, dead in the betamacs bin
pub const APP_APP: &str = "betamacs";

const ALG_ECDSA_P256_SHA256: &str = "ecdsa-p256-sha256";
/// ecdsa-with-SHA256 (certificate signature algorithm).
const OID_ECDSA_SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
const OID_EXT_KEY_USAGE: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.37");
const OID_BASIC_CONSTRAINTS: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.19");
const OID_KP_CODE_SIGNING: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.3");

/// otactl firmware manifest (wire shape of the `manifest` object).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub app: String,
    pub arch: String,
    pub version: String,
    #[serde(default)]
    pub epoch: Option<u64>,
    pub filename: String,
    pub sha256: String,
    pub released_at: String,
    #[serde(default)]
    pub build_dtm: Option<String>,
    #[serde(default)]
    pub git_hash: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub board: Option<String>,
    #[serde(default)]
    pub install_mode: Option<String>,
}

impl Manifest {
    /// The exact bytes otactl signed: Go `MarshalIndent(m, "", "  ")`
    /// plus `'\n'`. Field order is the Go struct order; empty/zero
    /// omitempty fields produce no line.
    pub fn canonical_json(&self) -> Vec<u8> {
        let mut lines: Vec<String> = Vec::new();
        let push_str = |lines: &mut Vec<String>, key: &str, value: &str| {
            if !value.is_empty() {
                lines.push(format!("  \"{key}\": \"{}\"", escape(value)));
            }
        };
        push_str(&mut lines, "app", &self.app);
        push_str(&mut lines, "arch", &self.arch);
        push_str(&mut lines, "version", &self.version);
        if let Some(epoch) = self.epoch
            && epoch != 0
        {
            lines.push(format!("  \"epoch\": {epoch}"));
        }
        push_str(&mut lines, "filename", &self.filename);
        push_str(&mut lines, "sha256", &self.sha256);
        push_str(&mut lines, "releasedAt", &self.released_at);
        push_str(&mut lines, "buildDtm", self.build_dtm.as_deref().unwrap_or(""));
        push_str(&mut lines, "gitHash", self.git_hash.as_deref().unwrap_or(""));
        push_str(&mut lines, "role", self.role.as_deref().unwrap_or(""));
        push_str(&mut lines, "format", self.format.as_deref().unwrap_or(""));
        push_str(&mut lines, "board", self.board.as_deref().unwrap_or(""));
        push_str(
            &mut lines,
            "installMode",
            self.install_mode.as_deref().unwrap_or(""),
        );
        format!("{{\n{}\n}}\n", lines.join(",\n")).into_bytes()
    }
}

/// Go encoding/json's HTML-safe string escaping.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A config envelope as delivered to betamacsd: otactl's manifest
/// response fields plus the artifact bytes inline.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub manifest: Manifest,
    pub signature: String,
    #[serde(default)]
    pub signature_algorithm: Option<String>,
    #[serde(default)]
    pub signing_certificate: Option<String>,
    #[serde(default)]
    pub signing_certificate_chain: Option<String>,
    /// base64 (std) of the config artifact (a betamacs package.json).
    pub artifact: String,
}

/// Outcome of a successful verification.
pub struct Verified {
    pub epoch: u64,
    pub artifact: Vec<u8>,
    /// For operator-facing logs (unused by betamacs itself so far).
    #[allow(dead_code)]
    pub version: String,
}

#[derive(Clone)]
pub struct Verifier {
    roots: Vec<Certificate>,
}

impl Verifier {
    /// Pin the otactl root(s) from PEM bytes.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        let roots = Certificate::load_pem_chain(pem).context("parse pinned root PEM")?;
        anyhow::ensure!(!roots.is_empty(), "pinned root PEM contains no certificates");
        Ok(Self { roots })
    }

    /// Load the pin from an app bundle's Resources (betamacsd's entry
    /// point). Its absence is what makes an install unmanaged.
    #[allow(dead_code)] // used by betamacsd, dead in the betamacs bin
    pub fn from_bundled_root(app: &Path) -> Result<Self> {
        let path = app.join("Contents/Resources/otactl-root.pem");
        let pem = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Self::from_pem(&pem)
    }

    /// Full envelope verification; `last_epoch` is the persisted
    /// high-water for rollback refusal, `expected_app` the otactl app
    /// name this envelope must be for.
    pub fn verify(&self, env: &Envelope, last_epoch: u64, expected_app: &str) -> Result<Verified> {
        if let Some(alg) = env.signature_algorithm.as_deref()
            && !alg.is_empty()
            && alg != ALG_ECDSA_P256_SHA256
        {
            bail!("unsupported signature algorithm {alg:?}");
        }

        let leaf_pem = env
            .signing_certificate
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("no signing certificate")?;
        let leaf = Certificate::load_pem_chain(leaf_pem.as_bytes())
            .context("parse signing certificate")?
            .into_iter()
            .next()
            .context("signing certificate PEM contains no certificate")?;
        let chain = match env.signing_certificate_chain.as_deref() {
            Some(pem) if !pem.trim().is_empty() => {
                Certificate::load_pem_chain(pem.as_bytes()).context("parse certificate chain")?
            }
            _ => Vec::new(),
        };

        self.verify_chain(&leaf, &chain)?;
        require_code_signing_eku(&leaf)?;

        // Manifest signature over the canonical bytes.
        let spki = leaf
            .tbs_certificate
            .subject_public_key_info
            .subject_public_key
            .as_bytes()
            .context("leaf public key is not byte-aligned")?;
        let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(spki)
            .map_err(|e| anyhow::anyhow!("signing key is not P-256: {e}"))?;
        let sig_der = base64::engine::general_purpose::STANDARD
            .decode(env.signature.trim())
            .context("signature is not base64")?;
        let sig = p256::ecdsa::Signature::from_der(&sig_der)
            .map_err(|e| anyhow::anyhow!("signature is not DER ECDSA: {e}"))?;
        key.verify(&env.manifest.canonical_json(), &sig)
            .map_err(|_| anyhow::anyhow!("manifest signature verification failed"))?;

        // Identity, artifact binding, rollback.
        anyhow::ensure!(
            env.manifest.app == expected_app,
            "manifest is for app {:?}, expected {expected_app:?}",
            env.manifest.app,
        );
        let artifact = base64::engine::general_purpose::STANDARD
            .decode(env.artifact.trim())
            .context("artifact is not base64")?;
        let digest = format!("{:x}", sha2::Sha256::digest(&artifact));
        anyhow::ensure!(
            digest == env.manifest.sha256.to_lowercase(),
            "artifact sha256 mismatch (got {digest}, manifest {})",
            env.manifest.sha256,
        );
        if let Some(epoch) = env.manifest.epoch
            && epoch < last_epoch
        {
            bail!("rollback refused: epoch {epoch} is below the accepted {last_epoch}");
        }
        Ok(Verified {
            // Never lower the persisted high-water (absent epoch = 0).
            epoch: env.manifest.epoch.unwrap_or(0).max(last_epoch),
            artifact,
            version: env.manifest.version.clone(),
        })
    }

    /// Anchors-only path validation: leaf -> (chain certs) -> a pinned
    /// root, verifying each signature, validity window, and issuer CA
    /// flag along the way. Mirrors the Swift client's SecTrust basic
    /// X.509 policy with SetAnchorCertificatesOnly(true).
    fn verify_chain(&self, leaf: &Certificate, chain: &[Certificate]) -> Result<()> {
        let mut cur = leaf;
        for _depth in 0..5 {
            check_validity(cur)?;
            let issuer_name = &cur.tbs_certificate.issuer;
            if let Some(root) = self
                .roots
                .iter()
                .find(|r| &r.tbs_certificate.subject == issuer_name)
            {
                check_validity(root)?;
                verify_cert_signature(cur, root)?;
                return Ok(());
            }
            let issuer = chain
                .iter()
                .find(|c| &c.tbs_certificate.subject == issuer_name)
                .with_context(|| format!("no issuer found for {}", cur.tbs_certificate.subject))?;
            require_ca(issuer)?;
            verify_cert_signature(cur, issuer)?;
            cur = issuer;
        }
        bail!("certificate chain too deep");
    }
}

fn check_validity(cert: &Certificate) -> Result<()> {
    let validity = &cert.tbs_certificate.validity;
    let now = SystemTime::now();
    let not_before = validity.not_before.to_system_time();
    let not_after = validity.not_after.to_system_time();
    anyhow::ensure!(
        now >= not_before && now <= not_after,
        "certificate {} outside validity window",
        cert.tbs_certificate.subject,
    );
    Ok(())
}

/// Verify `cert`'s signature with `issuer`'s P-256 key
/// (ecdsa-with-SHA256 only, matching the otactl PKI).
fn verify_cert_signature(cert: &Certificate, issuer: &Certificate) -> Result<()> {
    anyhow::ensure!(
        cert.signature_algorithm.oid == OID_ECDSA_SHA256,
        "certificate {} uses unsupported signature algorithm {}",
        cert.tbs_certificate.subject,
        cert.signature_algorithm.oid,
    );
    let tbs = cert.tbs_certificate.to_der().context("re-encode TBS")?;
    let spki = issuer
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("issuer public key is not byte-aligned")?;
    let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(spki)
        .map_err(|e| anyhow::anyhow!("issuer key is not P-256: {e}"))?;
    let sig_der = cert
        .signature
        .as_bytes()
        .context("certificate signature is not byte-aligned")?;
    let sig = p256::ecdsa::Signature::from_der(sig_der)
        .map_err(|e| anyhow::anyhow!("certificate signature is not DER: {e}"))?;
    key.verify(&tbs, &sig).map_err(|_| {
        anyhow::anyhow!(
            "signature of {} does not verify against {}",
            cert.tbs_certificate.subject,
            issuer.tbs_certificate.subject,
        )
    })
}

fn find_extension<'a>(cert: &'a Certificate, oid: &ObjectIdentifier) -> Option<&'a [u8]> {
    cert.tbs_certificate
        .extensions
        .as_ref()?
        .iter()
        .find(|e| &e.extn_id == oid)
        .map(|e| e.extn_value.as_bytes())
}

/// The leaf must carry id-kp-codeSigning; a leaf without an EKU
/// extension is rejected (matching the Swift client).
fn require_code_signing_eku(leaf: &Certificate) -> Result<()> {
    let value =
        find_extension(leaf, &OID_EXT_KEY_USAGE).context("signing certificate has no EKU")?;
    let ekus: Vec<ObjectIdentifier> =
        Decode::from_der(value).context("parse EKU extension")?;
    anyhow::ensure!(
        ekus.contains(&OID_KP_CODE_SIGNING),
        "signing certificate lacks the code-signing EKU",
    );
    Ok(())
}

fn require_ca(cert: &Certificate) -> Result<()> {
    let value = find_extension(cert, &OID_BASIC_CONSTRAINTS)
        .with_context(|| format!("issuer {} has no basicConstraints", cert.tbs_certificate.subject))?;
    let bc: x509_cert::ext::pkix::BasicConstraints =
        Decode::from_der(value).context("parse basicConstraints")?;
    anyhow::ensure!(
        bc.ca,
        "issuer {} is not a CA",
        cert.tbs_certificate.subject,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector from otactl's service_epoch_test.go.
    #[test]
    fn canonical_matches_go() {
        let m = Manifest {
            app: "rotec-runtime".into(),
            arch: "pico-w".into(),
            version: "1.2.0".into(),
            epoch: Some(3),
            filename: "runtime-1.2.0.bin".into(),
            sha256: "1a5f8c1e9c5a4d2f6b3e7a0c9d8b2e4f6a1c3d5e7f9b0a2c4d6e8f0a1b3c5d7e".into(),
            released_at: "2026-08-19T00:00:00Z".into(),
            build_dtm: None,
            git_hash: None,
            role: None,
            format: None,
            board: None,
            install_mode: None,
        };
        let want = "{\n  \"app\": \"rotec-runtime\",\n  \"arch\": \"pico-w\",\n  \"version\": \"1.2.0\",\n  \"epoch\": 3,\n  \"filename\": \"runtime-1.2.0.bin\",\n  \"sha256\": \"1a5f8c1e9c5a4d2f6b3e7a0c9d8b2e4f6a1c3d5e7f9b0a2c4d6e8f0a1b3c5d7e\",\n  \"releasedAt\": \"2026-08-19T00:00:00Z\"\n}\n";
        assert_eq!(String::from_utf8(m.canonical_json()).unwrap(), want);
    }

    #[test]
    fn escape_matches_go() {
        assert_eq!(
            escape("a <b> & \"c\".zip"),
            "a \\u003cb\\u003e \\u0026 \\\"c\\\".zip"
        );
        assert_eq!(escape("tab\there"), "tab\\there");
        assert_eq!(escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn epoch_zero_is_omitted() {
        let m = Manifest {
            app: "a".into(),
            arch: "b".into(),
            version: "1".into(),
            epoch: Some(0),
            filename: "f".into(),
            sha256: "s".into(),
            released_at: "t".into(),
            build_dtm: None,
            git_hash: None,
            role: Some("runtime".into()),
            format: None,
            board: None,
            install_mode: None,
        };
        let text = String::from_utf8(m.canonical_json()).unwrap();
        assert!(!text.contains("epoch"));
        assert!(text.contains("\"role\": \"runtime\""));
    }
}
