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

/// The otactl app names envelopes may carry: settings, the .app zip, and
/// the challenge task bank.
pub const CONFIG_APP: &str = "betamacs-config";
#[allow(dead_code)] // used by betamacsd, dead in the betamacs bin
pub const APP_APP: &str = "betamacs";
/// The challenge task bank — a separate, independently-versioned artifact.
/// Author-signed like config (a bad bank can lock a kid out).
#[allow(dead_code)] // used by betamacsd, dead in the betamacs bin
pub const TASKS_APP: &str = "betamacs-tasks";

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
    /// RFC3339 `authoredAt` from the author wrapper, or None when the artifact
    /// is not author-wrapped (e.g. the app zip). The daemon uses it for
    /// generation-monotonic rollback refusal (a stashed old signing can't be
    /// re-uploaded over a newer one) — the meaningful replacement for the old
    /// `notAfter` expiry, which is no longer enforced. Read by betamacsd; the
    /// betamacs agent bin shares this module but doesn't use it.
    #[allow(dead_code)]
    pub authored_at: Option<String>,
}

/// The author wrapper a config artifact must be when an author key is
/// pinned (docs/managed-mode.md, "integral timed locks"): the raw
/// package bytes plus a signature by the policy-author key — a key
/// otactl does not hold, so no server-side path can mint policy. The
/// validity window stops pre-signed stashes from outliving their
/// authoring session.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorWrapper {
    /// base64 (std) of the exact package.json bytes that were signed.
    pub package_b64: String,
    /// RFC3339 UTC; informational, covered by the signature.
    pub authored_at: String,
    /// v1 only: RFC3339 UTC expiry, kept in the signed bytes. No longer
    /// enforced; absent on v2 wrappers, which drop it entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    /// base64 DER ECDSA P-256/SHA-256 over `signing_input()`.
    pub author_signature: String,
}

impl AuthorWrapper {
    /// The signed bytes: fields joined by newlines — exact strings, no
    /// canonicalization to agree on. The presence of `not_after` selects the
    /// format so both are verifiable: v1 (with the expiry field) and v2 (the
    /// current format, which drops it — authorship doesn't expire, and the
    /// daemon's authoredAt high-water is the anti-rollback guard).
    pub fn signing_input(&self) -> Vec<u8> {
        match &self.not_after {
            Some(not_after) => format!(
                "betamacs-config-author-v1\n{}\n{}\n{}",
                self.authored_at, not_after, self.package_b64
            ),
            None => format!(
                "betamacs-config-author-v2\n{}\n{}",
                self.authored_at, self.package_b64
            ),
        }
        .into_bytes()
    }
}

fn parse_rfc3339_utc(s: &str) -> Result<SystemTime> {
    // "YYYY-MM-DDTHH:MM:SSZ" only — what our own tooling writes.
    let s = s.trim();
    let fail = || anyhow::anyhow!("timestamp {s:?} is not YYYY-MM-DDTHH:MM:SSZ");
    let b = s.as_bytes();
    if b.len() != 20 || b[19] != b'Z' || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return Err(fail());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64> {
        s[r].parse::<i64>().map_err(|_| fail())
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    anyhow::ensure!((1..=12).contains(&mo) && (1..=31).contains(&d), fail());
    // Days since Unix epoch (civil-from-days inverse, Hinnant's algorithm).
    let (y2, mo2) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * mo2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    anyhow::ensure!(secs >= 0, fail());
    Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

#[derive(Clone)]
pub struct Verifier {
    roots: Vec<Certificate>,
    /// When pinned, config artifacts MUST be valid author wrappers.
    author_key: Option<p256::ecdsa::VerifyingKey>,
}

impl Verifier {
    /// Pin the otactl root(s) from PEM bytes.
    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        let roots = Certificate::load_pem_chain(pem).context("parse pinned root PEM")?;
        anyhow::ensure!(!roots.is_empty(), "pinned root PEM contains no certificates");
        Ok(Self {
            roots,
            author_key: None,
        })
    }

    /// Additionally pin the policy-author public key (SPKI PEM): config
    /// artifacts are then accepted only as valid author wrappers.
    pub fn with_author_key_pem(mut self, pem: &str) -> Result<Self> {
        use p256::pkcs8::DecodePublicKey;
        let key = p256::PublicKey::from_public_key_pem(pem)
            .map_err(|e| anyhow::anyhow!("parse author public key: {e}"))?;
        self.author_key = Some(p256::ecdsa::VerifyingKey::from(key));
        Ok(self)
    }

    /// Load the pins from an app bundle's Resources (betamacsd's entry
    /// point). No otactl root = unmanaged; an author-pubkey.pem beside
    /// it additionally requires author-signed config.
    #[allow(dead_code)] // used by betamacsd, dead in the betamacs bin
    pub fn from_bundled_root(app: &Path) -> Result<Self> {
        let resources = app.join("Contents/Resources");
        let pem = std::fs::read(resources.join("otactl-root.pem"))
            .with_context(|| format!("read {}", resources.join("otactl-root.pem").display()))?;
        let verifier = Self::from_pem(&pem)?;
        match std::fs::read_to_string(resources.join("author-pubkey.pem")) {
            Ok(author) => verifier.with_author_key_pem(&author),
            Err(_) => Ok(verifier),
        }
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

        // Integral policy authorship: with an author key pinned, a config
        // or task-bank artifact must be a wrapper signed by the policy
        // author — a key otactl never holds, so no server-side path can
        // mint policy or the challenges that gate the network.
        let (artifact, authored_at) = if (expected_app == CONFIG_APP
            || expected_app == TASKS_APP)
            && self.author_key.is_some()
        {
            let (bytes, at) = self.unwrap_authored(&artifact)?;
            (bytes, Some(at))
        } else {
            (artifact, None)
        };
        Ok(Verified {
            // Never lower the persisted high-water (absent epoch = 0).
            epoch: env.manifest.epoch.unwrap_or(0).max(last_epoch),
            artifact,
            version: env.manifest.version.clone(),
            authored_at,
        })
    }

    /// Verify the author wrapper and return the inner package bytes plus the
    /// wrapper's `authoredAt` (for generation-monotonic rollback refusal).
    ///
    /// `notAfter` is intentionally NOT enforced as an expiry: an author
    /// signature attests authorship, which does not expire, and expiring it
    /// created a failure mode where a valid config became un-appliable and the
    /// device silently coasted on stale policy. Anti-rollback is the epoch plus
    /// the `authoredAt` high-water (enforced by the daemon). `notAfter` stays in
    /// the wrapper only because it is covered by the existing signature; a v2
    /// wrapper will drop it once the fleet no longer runs a build that expects
    /// it in the signing input.
    fn unwrap_authored(&self, artifact: &[u8]) -> Result<(Vec<u8>, String)> {
        let key = self.author_key.as_ref().expect("caller checked");
        let wrapper: AuthorWrapper = serde_json::from_slice(artifact)
            .context("config is not an author-signed wrapper (author key is pinned)")?;
        // Validate authoredAt is well-formed so the daemon's lexical high-water
        // comparison is chronological.
        parse_rfc3339_utc(&wrapper.authored_at)
            .context("author wrapper authoredAt is not a valid RFC3339 UTC timestamp")?;
        let sig_der = base64::engine::general_purpose::STANDARD
            .decode(wrapper.author_signature.trim())
            .context("author signature is not base64")?;
        let sig = p256::ecdsa::Signature::from_der(&sig_der)
            .map_err(|e| anyhow::anyhow!("author signature is not DER ECDSA: {e}"))?;
        key.verify(&wrapper.signing_input(), &sig)
            .map_err(|_| anyhow::anyhow!("author signature verification failed"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(wrapper.package_b64.trim())
            .context("wrapper packageB64 is not base64")?;
        Ok((bytes, wrapper.authored_at))
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
    fn rfc3339_parser_matches_known_epochs() {
        let t = |s: &str| {
            parse_rfc3339_utc(s)
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(t("1970-01-01T00:00:00Z"), 0);
        assert_eq!(t("2026-09-03T12:00:00Z"), 1788436800);
        assert_eq!(t("2000-03-01T00:00:00Z"), 951868800);
        assert!(parse_rfc3339_utc("2026-09-03 12:00:00").is_err());
        assert!(parse_rfc3339_utc("garbage").is_err());
    }

    fn wrapper_for(package: &[u8], not_after: Option<&str>) -> (AuthorWrapper, Verifier) {
        use base64::Engine;
        use p256::ecdsa::signature::Signer;
        use p256::pkcs8::EncodePublicKey;
        let signing = p256::ecdsa::SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let mut wrapper = AuthorWrapper {
            package_b64: base64::engine::general_purpose::STANDARD.encode(package),
            authored_at: "2026-09-03T00:00:00Z".into(),
            not_after: not_after.map(|s| s.into()),
            author_signature: String::new(),
        };
        let sig: p256::ecdsa::Signature = signing.sign(&wrapper.signing_input());
        wrapper.author_signature =
            base64::engine::general_purpose::STANDARD.encode(sig.to_der().as_bytes());
        let pem = signing
            .verifying_key()
            .to_public_key_pem(Default::default())
            .unwrap();
        let verifier = Verifier {
            roots: Vec::new(),
            author_key: None,
        }
        .with_author_key_pem(&pem)
        .unwrap();
        (wrapper, verifier)
    }

    #[test]
    fn author_wrapper_round_trip() {
        let package = br#"{"layers":[]}"#;
        // v1 (with notAfter) and v2 (without) both verify.
        for not_after in [Some("2099-01-01T00:00:00Z"), None] {
            let (wrapper, verifier) = wrapper_for(package, not_after);
            let artifact = serde_json::to_vec(&wrapper).unwrap();
            let (bytes, authored) = verifier.unwrap_authored(&artifact).unwrap();
            assert_eq!(bytes, package.to_vec());
            assert_eq!(authored, "2026-09-03T00:00:00Z");
        }
        // A v2 wrapper must not serialize a notAfter field at all.
        let (wrapper, _) = wrapper_for(package, None);
        let json = serde_json::to_string(&wrapper).unwrap();
        assert!(!json.contains("notAfter"), "v2 wrapper leaked notAfter: {json}");
    }

    #[test]
    fn author_wrapper_ignores_expiry_but_rejects_tamper() {
        let package = br#"{"layers":[]}"#;
        // notAfter is no longer enforced: a v1 wrapper whose window is long past
        // still verifies (authorship doesn't expire; anti-rollback is the epoch
        // + the daemon's authoredAt high-water).
        let (old, verifier) = wrapper_for(package, Some("2020-01-01T00:00:00Z"));
        let (bytes, _) = verifier
            .unwrap_authored(&serde_json::to_vec(&old).unwrap())
            .expect("expired notAfter must no longer be rejected");
        assert_eq!(bytes, package.to_vec());

        let (mut tampered, verifier) = wrapper_for(package, Some("2099-01-01T00:00:00Z"));
        use base64::Engine;
        tampered.package_b64 =
            base64::engine::general_purpose::STANDARD.encode(br#"{"layers":["evil"]}"#);
        let err = verifier
            .unwrap_authored(&serde_json::to_vec(&tampered).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("verification failed"), "{err}");

        // A plain (unwrapped) config must be refused when a key is pinned.
        assert!(verifier.unwrap_authored(package).is_err());
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
