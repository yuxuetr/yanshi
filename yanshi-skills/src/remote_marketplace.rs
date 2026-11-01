//! Unified remote marketplace manifest for Skills and Plugins.
//!
//! This schema is intentionally package-type neutral. It is the catalog format
//! the future `yanshi marketplace ...` CLI will fetch over read-only HTTP
//! before installing either a Skill package or a Plugin package.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::SkillError;

pub const DEFAULT_REMOTE_MARKETPLACE_SCHEMA_VERSION: u32 = 1;

/// Default upper bound on a remote marketplace manifest body. 1 MiB is
/// well above any realistic skill / plugin manifest (the largest in
/// the test fixtures is < 8 KiB) and matches the cost of holding the
/// bytes in memory. Q1.10.2.
pub const DEFAULT_MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Default upper bound on a downloaded marketplace artifact. 256 MiB
/// is generous enough for plugin tarballs that ship native binaries
/// while still capping the memory pressure of a misbehaving server
/// streaming infinite bytes. Q1.10.2.
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Read-only HTTP client for remote marketplace registries.
#[derive(Clone)]
pub struct RemoteMarketplaceClient {
  client: reqwest::Client,
  /// Q1.10.2: ceiling on a manifest body. Servers that exceed this
  /// see a `SkillError::HttpError` instead of an unbounded read.
  max_manifest_bytes: u64,
  /// Q1.10.2: ceiling on an artifact body.
  max_artifact_bytes: u64,
}

impl std::fmt::Debug for RemoteMarketplaceClient {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RemoteMarketplaceClient")
      .finish_non_exhaustive()
  }
}

impl Default for RemoteMarketplaceClient {
  fn default() -> Self {
    Self::new()
  }
}

impl RemoteMarketplaceClient {
  pub fn new() -> Self {
    // Avoid platform proxy discovery. It can touch OS services that are not
    // available in sandboxed CLI/test environments, and marketplace registry
    // URLs are explicit.
    // `Client::builder().no_proxy().build()` only fails on TLS backend init
    // (e.g. missing native-tls on Linux); the test suite for this crate
    // exercises the constructor on macOS / Linux CI, so a runtime panic
    // here means the build is mis-configured (Q5.1).
    #[allow(
      clippy::expect_used,
      reason = "reqwest Client::builder().no_proxy().build() is infallible on supported targets; covered by remote_marketplace tests"
    )]
    let client = reqwest::Client::builder()
      .no_proxy()
      .build()
      .expect("reqwest client builder with no_proxy should be infallible");
    Self {
      client,
      max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
      max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
    }
  }

  pub fn with_client(client: reqwest::Client) -> Self {
    Self {
      client,
      max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
      max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
    }
  }

  /// Override the manifest body size cap. Q1.10.2.
  pub fn with_max_manifest_bytes(mut self, max: u64) -> Self {
    self.max_manifest_bytes = max;
    self
  }

  /// Override the artifact body size cap. Q1.10.2.
  pub fn with_max_artifact_bytes(mut self, max: u64) -> Self {
    self.max_artifact_bytes = max;
    self
  }

  /// Fetch a remote marketplace manifest over HTTP(S), then parse and validate
  /// it. This method is read-only: it never writes cache state or installs
  /// packages.
  pub async fn fetch_manifest(&self, url: &str) -> Result<RemoteMarketplaceManifest, SkillError> {
    validate_registry_url(url)?;
    let response = self
      .client
      .get(url)
      .header(reqwest::header::ACCEPT, "application/toml, text/plain, */*")
      .send()
      .await
      .map_err(|err| SkillError::HttpError(format!("failed to fetch '{}': {}", url, err)))?;
    let status = response.status();
    if !status.is_success() {
      return Err(SkillError::HttpError(format!(
        "failed to fetch '{}': HTTP {}",
        url, status
      )));
    }
    // Q1.10.2: refuse manifests that the server advertises as larger
    // than our cap (pre-check via Content-Length) AND verify the
    // bytes we actually read also stay under the cap (some servers
    // lie about Content-Length, or stream chunked).
    if let Some(announced) = content_length(&response)
      && announced > self.max_manifest_bytes
    {
      return Err(SkillError::HttpError(format!(
        "manifest at '{}' announces {} bytes, exceeding cap {}",
        url, announced, self.max_manifest_bytes
      )));
    }
    let bytes = response.bytes().await.map_err(|err| {
      SkillError::HttpError(format!(
        "failed to read response body from '{}': {}",
        url, err
      ))
    })?;
    if bytes.len() as u64 > self.max_manifest_bytes {
      return Err(SkillError::HttpError(format!(
        "manifest at '{}' streamed {} bytes, exceeding cap {}",
        url,
        bytes.len(),
        self.max_manifest_bytes
      )));
    }
    let body = std::str::from_utf8(&bytes).map_err(|err| {
      SkillError::HttpError(format!("manifest at '{}' is not valid UTF-8: {}", url, err))
    })?;
    RemoteMarketplaceManifest::parse_toml(body)
  }

  /// Fetch package bytes from an artifact URL. Callers should pass the bytes
  /// through [`RemoteMarketplaceCache`] before using them.
  ///
  /// Q1.10.2: honors the `max_artifact_bytes` cap (default 256 MiB)
  /// and ships an optional `If-None-Match: <etag>` header so callers
  /// that already have a cached copy can short-circuit on `304 Not
  /// Modified`.
  pub async fn fetch_artifact_bytes(&self, url: &str) -> Result<Vec<u8>, SkillError> {
    self
      .fetch_artifact_bytes_with_etag(url, None)
      .await
      .map(|out| out.bytes)
  }

  pub async fn fetch_artifact_bytes_with_etag(
    &self,
    url: &str,
    if_none_match: Option<&str>,
  ) -> Result<ArtifactFetchOutcome, SkillError> {
    validate_registry_url(url)?;
    let mut request = self.client.get(url);
    if let Some(etag) = if_none_match {
      request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request
      .send()
      .await
      .map_err(|err| SkillError::HttpError(format!("failed to fetch '{}': {}", url, err)))?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
      return Ok(ArtifactFetchOutcome {
        bytes: Vec::new(),
        etag: if_none_match.map(|s| s.to_string()),
        not_modified: true,
      });
    }
    if !status.is_success() {
      return Err(SkillError::HttpError(format!(
        "failed to fetch '{}': HTTP {}",
        url, status
      )));
    }
    if let Some(announced) = content_length(&response)
      && announced > self.max_artifact_bytes
    {
      return Err(SkillError::HttpError(format!(
        "artifact at '{}' announces {} bytes, exceeding cap {}",
        url, announced, self.max_artifact_bytes
      )));
    }
    let etag = response
      .headers()
      .get(reqwest::header::ETAG)
      .and_then(|v| v.to_str().ok())
      .map(|s| s.to_string());
    let bytes = response.bytes().await.map_err(|err| {
      SkillError::HttpError(format!(
        "failed to read artifact body from '{}': {}",
        url, err
      ))
    })?;
    if bytes.len() as u64 > self.max_artifact_bytes {
      return Err(SkillError::HttpError(format!(
        "artifact at '{}' streamed {} bytes, exceeding cap {}",
        url,
        bytes.len(),
        self.max_artifact_bytes
      )));
    }
    Ok(ArtifactFetchOutcome {
      bytes: bytes.to_vec(),
      etag,
      not_modified: false,
    })
  }
}

/// Result of a [`RemoteMarketplaceClient::fetch_artifact_bytes_with_etag`]
/// call. `not_modified` is `true` when the server returned 304 in
/// response to the `If-None-Match` header — the cached copy is
/// authoritative and `bytes` is empty.
#[derive(Debug, Clone)]
pub struct ArtifactFetchOutcome {
  pub bytes: Vec<u8>,
  pub etag: Option<String>,
  pub not_modified: bool,
}

fn content_length(response: &reqwest::Response) -> Option<u64> {
  response.content_length()
}

/// Local cache for downloaded marketplace artifacts.
#[derive(Debug, Clone)]
pub struct RemoteMarketplaceCache {
  root: PathBuf,
  client: RemoteMarketplaceClient,
  signature_verifier: Arc<dyn MarketplaceSignatureVerifier>,
}

impl RemoteMarketplaceCache {
  pub fn new(root: impl Into<PathBuf>) -> Self {
    // V3.5: default to real Ed25519 verification instead of a
    // self-computed checksum re-hash (which only proves the manifest
    // and artifact agree, not that either came from a trusted
    // publisher). `with_require_signature(false)` preserves this
    // constructor's existing "no [signature] block => Unsigned, Ok"
    // behavior — callers that want to *require* every entry to be
    // signed still opt in explicitly via
    // `with_client_and_verifier(..., Ed25519SignatureVerifier::new(..))`
    // (the default `require_signature: true`), same as
    // `yanshi-cli`'s `cache_from_dir` already does for remote
    // registries.
    Self::with_client_and_verifier(
      root,
      RemoteMarketplaceClient::new(),
      Arc::new(
        Ed25519SignatureVerifier::new(Ed25519SignatureVerifier::default_keys_dir())
          .with_require_signature(false),
      ),
    )
  }

  pub fn with_client_and_verifier(
    root: impl Into<PathBuf>,
    client: RemoteMarketplaceClient,
    signature_verifier: Arc<dyn MarketplaceSignatureVerifier>,
  ) -> Self {
    Self {
      root: root.into(),
      client,
      signature_verifier,
    }
  }

  pub fn default_root() -> PathBuf {
    dirs::home_dir()
      .unwrap_or_else(|| PathBuf::from("."))
      .join(".yanshi")
      .join("marketplace")
      .join("cache")
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  pub fn artifact_path(&self, entry: &RemoteMarketplaceEntry) -> Result<PathBuf, SkillError> {
    let checksum = entry.source.normalized_checksum()?;
    Ok(
      self
        .root
        .join("artifacts")
        .join(entry.package_type.as_str())
        .join(sanitize_path_segment(&entry.name))
        .join(sanitize_path_segment(&entry.version))
        .join(format!("{checksum}.pkg")),
    )
  }

  pub fn is_cached(&self, entry: &RemoteMarketplaceEntry) -> Result<bool, SkillError> {
    Ok(self.artifact_path(entry)?.is_file())
  }

  pub fn verify_cached_artifact(
    &self,
    entry: &RemoteMarketplaceEntry,
  ) -> Result<CachedMarketplaceArtifact, SkillError> {
    let path = self.artifact_path(entry)?;
    let bytes = fs::read(&path).map_err(|err| {
      SkillError::IoError(format!(
        "failed to read cached artifact '{}': {}",
        path.display(),
        err
      ))
    })?;
    let expected = entry.source.normalized_checksum()?;
    let actual = sha256_bytes(&bytes);
    if actual != expected {
      return Err(validation_error(format!(
        "Cached artifact checksum mismatch for '{}@{}' (expected {}, got {})",
        entry.name, entry.version, expected, actual
      )));
    }
    let signature_verification = self.signature_verifier.verify(entry, &bytes)?;
    Ok(CachedMarketplaceArtifact {
      entry_name: entry.name.clone(),
      version: entry.version.clone(),
      package_type: entry.package_type,
      path,
      checksum_sha256: actual,
      signature_checked: entry.signature.is_some(),
      signature_verification,
    })
  }

  pub async fn fetch_and_cache_artifact(
    &self,
    entry: &RemoteMarketplaceEntry,
  ) -> Result<CachedMarketplaceArtifact, SkillError> {
    let bytes = self
      .client
      .fetch_artifact_bytes(&entry.source.artifact_url)
      .await?;
    self.cache_artifact_bytes(entry, &bytes)
  }

  pub fn cache_artifact_bytes(
    &self,
    entry: &RemoteMarketplaceEntry,
    bytes: &[u8],
  ) -> Result<CachedMarketplaceArtifact, SkillError> {
    entry.validate()?;
    let expected = entry.source.normalized_checksum()?;
    let actual = sha256_bytes(bytes);
    if actual != expected {
      return Err(validation_error(format!(
        "Artifact checksum mismatch for '{}@{}' (expected {}, got {})",
        entry.name, entry.version, expected, actual
      )));
    }
    let signature_verification = self.signature_verifier.verify(entry, bytes)?;

    let path = self.artifact_path(entry)?;
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("pkg.tmp");
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, &path)?;

    Ok(CachedMarketplaceArtifact {
      entry_name: entry.name.clone(),
      version: entry.version.clone(),
      package_type: entry.package_type,
      path,
      checksum_sha256: actual,
      signature_checked: entry.signature.is_some(),
      signature_verification,
    })
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedMarketplaceArtifact {
  pub entry_name: String,
  pub version: String,
  pub package_type: MarketplacePackageType,
  pub path: PathBuf,
  pub checksum_sha256: String,
  pub signature_checked: bool,
  /// T0.1: what kind of check `signature_checked` actually represents —
  /// distinguishes a real Ed25519 signature from a self-reported checksum
  /// re-hash so callers can't accidentally treat the latter as the former.
  pub signature_verification: SignatureVerificationKind,
}

/// What kind of check a [`MarketplaceSignatureVerifier`] actually performed.
///
/// T0.1: `signature_checked` on [`CachedMarketplaceArtifact`] only answers
/// "was there a `[signature]` block that matched" — it can't distinguish a
/// real cryptographic signature from a self-reported checksum re-hash
/// (which an attacker who controls the artifact can trivially recompute).
/// This type carries that distinction explicitly so callers don't have to
/// infer it from which verifier they happened to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureVerificationKind {
  /// No `[signature]` block was present, and the active verifier did not
  /// require one.
  Unsigned,
  /// Only a self-reported SHA-256 checksum was checked. **Not a real
  /// signature** — it proves the manifest and artifact agree, not that
  /// either came from a trusted publisher.
  ChecksumOnly,
  /// Verified against a real Ed25519 cryptographic signature.
  CryptographicSignature,
}

impl SignatureVerificationKind {
  /// True only when a genuine cryptographic signature was verified.
  pub fn is_cryptographic(self) -> bool {
    matches!(self, Self::CryptographicSignature)
  }
}

impl std::fmt::Display for SignatureVerificationKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let label = match self {
      Self::Unsigned => "unsigned",
      Self::ChecksumOnly => "checksum_only",
      Self::CryptographicSignature => "cryptographic_signature",
    };
    write!(f, "{label}")
  }
}

/// Pluggable marketplace signature verifier.
///
/// The built-in verifier below exists for deterministic local tests and
/// bootstrap registries. Production registries should install a verifier for
/// their chosen signature system (for example minisign or sigstore).
pub trait MarketplaceSignatureVerifier: Send + Sync + std::fmt::Debug {
  fn verify(
    &self,
    entry: &RemoteMarketplaceEntry,
    artifact: &[u8],
  ) -> Result<SignatureVerificationKind, SkillError>;
}

#[derive(Debug, Clone, Default)]
pub struct ChecksumSha256SignatureVerifier;

impl MarketplaceSignatureVerifier for ChecksumSha256SignatureVerifier {
  fn verify(
    &self,
    entry: &RemoteMarketplaceEntry,
    artifact: &[u8],
  ) -> Result<SignatureVerificationKind, SkillError> {
    let Some(signature) = &entry.signature else {
      return Ok(SignatureVerificationKind::Unsigned);
    };
    let algorithm = signature.algorithm.trim().to_ascii_lowercase();
    if algorithm != "checksum-sha256" && algorithm != "sha256" {
      return Err(validation_error(format!(
        "Unsupported signature algorithm '{}' for '{}@{}'",
        signature.algorithm, entry.name, entry.version
      )));
    }
    let expected = normalize_sha256(&signature.value)?;
    let actual = sha256_bytes(artifact);
    if actual != expected {
      return Err(validation_error(format!(
        "Signature checksum mismatch for '{}@{}' (expected {}, got {})",
        entry.name, entry.version, expected, actual
      )));
    }
    Ok(SignatureVerificationKind::ChecksumOnly)
  }
}

/// Q1.10.1: real Ed25519 signature verifier.
///
/// The pre-fix `ChecksumSha256SignatureVerifier` only re-computes
/// the artifact's SHA-256 and compares against the manifest field —
/// it is not a signature at all, just a self-checksum. An attacker
/// who modifies the artifact and recomputes the checksum trivially
/// passes verification. This struct loads Ed25519 public keys from a
/// configured directory (one PEM-or-base64 file per publisher,
/// keyed by `signature.key_id`) and verifies a base64-encoded
/// detached signature over the raw artifact bytes.
///
/// **Key format**: each file in `keys_dir` is named `<key_id>.pub`
/// and contains a single line of base64-encoded 32-byte raw Ed25519
/// public key material (`ED25519_PUBLIC_KEY_LENGTH = 32`). The
/// canonical command to produce one with `openssl` is:
///
/// ```text
/// openssl genpkey -algorithm ed25519 -out priv.pem
/// openssl pkey -in priv.pem -pubout -outform DER \
///     | tail -c 32 | base64 > <key_id>.pub
/// ```
///
/// **Manifest shape**: the corresponding entry in the marketplace
/// catalog references the same `key_id` and embeds a base64 detached
/// signature:
///
/// ```toml
/// [entries.signature]
/// algorithm = "ed25519"
/// key_id = "yuxuetr-publisher-2026"
/// value = "<base64 signature>"
/// ```
#[derive(Debug, Clone)]
pub struct Ed25519SignatureVerifier {
  keys_dir: PathBuf,
  /// When true, an entry without a `[signature]` block is rejected
  /// outright. Production deployments should leave this as true
  /// (the default); local dev / fixture tests can disable it to
  /// install unsigned packages.
  require_signature: bool,
}

impl Ed25519SignatureVerifier {
  pub fn new(keys_dir: impl Into<PathBuf>) -> Self {
    Self {
      keys_dir: keys_dir.into(),
      require_signature: true,
    }
  }

  /// Default key directory: `~/.yanshi/marketplace-keys/`. Falls
  /// back to `./.yanshi/marketplace-keys/` when no home dir is
  /// detectable (mostly CI sandbox setups).
  pub fn default_keys_dir() -> PathBuf {
    dirs::home_dir()
      .unwrap_or_else(|| PathBuf::from("."))
      .join(".yanshi")
      .join("marketplace-keys")
  }

  pub fn with_require_signature(mut self, require: bool) -> Self {
    self.require_signature = require;
    self
  }

  fn load_public_key(&self, key_id: &str) -> Result<ed25519_dalek::VerifyingKey, SkillError> {
    load_ed25519_public_key(&self.keys_dir, key_id)
  }
}

/// V3.5: shared Ed25519 key-loading logic, extracted from
/// `Ed25519SignatureVerifier::load_public_key` so
/// `yanshi-cli`'s plugin-signing path (a different manifest type,
/// `yanshi_core::plugin::manifest::PluginSignature`, not
/// [`MarketplaceSignature`]) can reuse the exact same key-file
/// format/validation instead of duplicating it.
pub fn load_ed25519_public_key(
  keys_dir: &Path,
  key_id: &str,
) -> Result<ed25519_dalek::VerifyingKey, SkillError> {
  use std::io::Read;

  // Q1.10.2-adjacent guard: `key_id` is used as a filename, so
  // refuse anything with `..` or path separators.
  if key_id.contains("..") || key_id.contains('/') || key_id.contains('\\') || key_id.is_empty() {
    return Err(validation_error(format!(
      "'{}' is not a valid filename component for a key_id",
      key_id
    )));
  }
  let path = keys_dir.join(format!("{}.pub", key_id));
  let mut file = fs::File::open(&path).map_err(|err| {
    validation_error(format!(
      "public key '{}' not found at {}: {}",
      key_id,
      path.display(),
      err
    ))
  })?;
  let mut content = String::new();
  file.read_to_string(&mut content).map_err(|err| {
    validation_error(format!(
      "failed to read public key {}: {}",
      path.display(),
      err
    ))
  })?;
  let bytes = base64_decode(content.trim())?;
  let key_bytes: [u8; ed25519_dalek::PUBLIC_KEY_LENGTH] =
    bytes.as_slice().try_into().map_err(|_| {
      validation_error(format!(
        "public key {} must be exactly {} bytes, got {}",
        path.display(),
        ed25519_dalek::PUBLIC_KEY_LENGTH,
        bytes.len()
      ))
    })?;
  ed25519_dalek::VerifyingKey::from_bytes(&key_bytes).map_err(|err| {
    validation_error(format!(
      "public key {} is malformed: {}",
      path.display(),
      err
    ))
  })
}

/// V3.5: shared Ed25519 signature-verification logic, extracted from
/// [`Ed25519SignatureVerifier::verify`] for the same reason as
/// [`load_ed25519_public_key`] above. `context` is used only to
/// produce a readable error message (e.g. `"plugin 'x'"` or
/// `"marketplace entry 'y@1.0.0'"`).
pub fn verify_ed25519_detached(
  public_key: &ed25519_dalek::VerifyingKey,
  artifact: &[u8],
  signature_b64: &str,
  context: &str,
) -> Result<(), SkillError> {
  use ed25519_dalek::Verifier;

  let sig_bytes = base64_decode(signature_b64.trim())?;
  let sig_array: [u8; ed25519_dalek::SIGNATURE_LENGTH] =
    sig_bytes.as_slice().try_into().map_err(|_| {
      validation_error(format!(
        "Ed25519 signature for {context} must be exactly {} bytes, got {}",
        ed25519_dalek::SIGNATURE_LENGTH,
        sig_bytes.len()
      ))
    })?;
  let sig = ed25519_dalek::Signature::from_bytes(&sig_array);
  public_key.verify(artifact, &sig).map_err(|err| {
    validation_error(format!(
      "Ed25519 signature verification failed for {context}: {err}"
    ))
  })
}

impl MarketplaceSignatureVerifier for Ed25519SignatureVerifier {
  fn verify(
    &self,
    entry: &RemoteMarketplaceEntry,
    artifact: &[u8],
  ) -> Result<SignatureVerificationKind, SkillError> {
    let Some(signature) = &entry.signature else {
      if self.require_signature {
        return Err(validation_error(format!(
          "marketplace entry '{}@{}' has no [signature] block but Ed25519 verifier requires one",
          entry.name, entry.version
        )));
      }
      return Ok(SignatureVerificationKind::Unsigned);
    };
    let algorithm = signature.algorithm.trim().to_ascii_lowercase();
    if algorithm != "ed25519" {
      return Err(validation_error(format!(
        "Ed25519 verifier rejected algorithm '{}' for '{}@{}'",
        signature.algorithm, entry.name, entry.version
      )));
    }
    let public_key = self.load_public_key(&signature.key_id)?;
    let context = format!("'{}@{}'", entry.name, entry.version);
    verify_ed25519_detached(&public_key, artifact, &signature.value, &context)?;
    Ok(SignatureVerificationKind::CryptographicSignature)
  }
}

fn base64_decode(input: &str) -> Result<Vec<u8>, SkillError> {
  use base64::Engine;
  base64::engine::general_purpose::STANDARD
    .decode(input)
    .map_err(|err| validation_error(format!("invalid base64 in marketplace data: {}", err)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMarketplaceManifest {
  #[serde(default = "default_schema_version")]
  pub schema_version: u32,
  pub name: String,
  #[serde(default)]
  pub description: Option<String>,
  #[serde(default)]
  pub homepage: Option<String>,
  #[serde(default)]
  pub entries: Vec<RemoteMarketplaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMarketplaceEntry {
  pub name: String,
  pub version: String,
  #[serde(rename = "type")]
  pub package_type: MarketplacePackageType,
  pub source: MarketplaceSource,
  #[serde(default)]
  pub signature: Option<MarketplaceSignature>,
  #[serde(default)]
  pub aliases: Vec<String>,
  #[serde(default)]
  pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketplacePackageType {
  Skill,
  Plugin,
}

impl MarketplacePackageType {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Skill => "skill",
      Self::Plugin => "plugin",
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSource {
  /// URL of the read-only registry document this entry came from.
  pub registry_url: String,
  /// URL of the package archive or repository snapshot to install.
  pub artifact_url: String,
  /// SHA-256 digest of the artifact. Accepts raw 64-char hex or
  /// `sha256:<hex>`; validation normalizes both forms.
  pub checksum_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSignature {
  pub algorithm: String,
  pub key_id: String,
  pub value: String,
}

impl RemoteMarketplaceManifest {
  pub fn load(path: &Path) -> Result<Self, SkillError> {
    let content = fs::read_to_string(path)?;
    Self::parse_toml(&content)
  }

  pub fn parse_toml(content: &str) -> Result<Self, SkillError> {
    let manifest: RemoteMarketplaceManifest = toml::from_str(content)?;
    manifest.validate()?;
    Ok(manifest)
  }

  pub fn validate(&self) -> Result<(), SkillError> {
    if self.schema_version != DEFAULT_REMOTE_MARKETPLACE_SCHEMA_VERSION {
      return Err(validation_error(format!(
        "Unsupported remote marketplace schema_version {} (expected {})",
        self.schema_version, DEFAULT_REMOTE_MARKETPLACE_SCHEMA_VERSION
      )));
    }
    if self.name.trim().is_empty() {
      return Err(validation_error(
        "Remote marketplace name must not be empty".to_string(),
      ));
    }
    if self.entries.is_empty() {
      return Err(validation_error(
        "Remote marketplace must contain at least one entry".to_string(),
      ));
    }

    let mut entry_keys = BTreeSet::new();
    let mut lookup_keys = BTreeSet::new();
    for entry in &self.entries {
      entry.validate()?;
      let entry_key = (
        entry.package_type,
        entry.name.to_ascii_lowercase(),
        entry.version.clone(),
      );
      if !entry_keys.insert(entry_key) {
        return Err(validation_error(format!(
          "Duplicate marketplace entry '{}@{}' for type {:?}",
          entry.name, entry.version, entry.package_type
        )));
      }

      let lookup_key = (entry.package_type, entry.name.to_ascii_lowercase());
      if !lookup_keys.insert(lookup_key) {
        return Err(validation_error(format!(
          "Duplicate marketplace package name '{}' for type {:?}",
          entry.name, entry.package_type
        )));
      }
      for alias in &entry.aliases {
        let alias = alias.trim();
        if alias.is_empty() {
          return Err(validation_error(format!(
            "Marketplace entry '{}' has an empty alias",
            entry.name
          )));
        }
        let alias_key = (entry.package_type, alias.to_ascii_lowercase());
        if !lookup_keys.insert(alias_key) {
          return Err(validation_error(format!(
            "Duplicate marketplace alias '{}' for type {:?}",
            alias, entry.package_type
          )));
        }
      }
    }
    Ok(())
  }

  pub fn entries(&self) -> &[RemoteMarketplaceEntry] {
    &self.entries
  }
}

impl RemoteMarketplaceEntry {
  pub fn validate(&self) -> Result<(), SkillError> {
    if self.name.trim().is_empty() {
      return Err(validation_error(
        "Marketplace entry name must not be empty".to_string(),
      ));
    }
    Version::parse(&self.version).map_err(|err| {
      validation_error(format!(
        "Invalid marketplace entry version '{}': {}",
        self.version, err
      ))
    })?;
    self.source.validate(&self.name)?;
    if let Some(signature) = &self.signature {
      signature.validate(&self.name)?;
    }
    Ok(())
  }
}

impl MarketplaceSource {
  pub fn normalized_checksum(&self) -> Result<String, SkillError> {
    normalize_sha256(&self.checksum_sha256)
  }

  fn validate(&self, entry_name: &str) -> Result<(), SkillError> {
    validate_http_url("registry_url", entry_name, &self.registry_url)?;
    validate_http_url("artifact_url", entry_name, &self.artifact_url)?;
    self.normalized_checksum()?;
    Ok(())
  }
}

impl MarketplaceSignature {
  fn validate(&self, entry_name: &str) -> Result<(), SkillError> {
    if self.algorithm.trim().is_empty() {
      return Err(validation_error(format!(
        "Marketplace entry '{}' signature algorithm must not be empty",
        entry_name
      )));
    }
    if self.key_id.trim().is_empty() {
      return Err(validation_error(format!(
        "Marketplace entry '{}' signature key_id must not be empty",
        entry_name
      )));
    }
    if self.value.trim().is_empty() {
      return Err(validation_error(format!(
        "Marketplace entry '{}' signature value must not be empty",
        entry_name
      )));
    }
    Ok(())
  }
}

/// V3.5: is `value` an `http://` (or `https://`) URL whose host is a
/// loopback address? Used to allow local dev/test registries
/// (`127.0.0.1`, `::1`, `localhost`) to keep using plain `http://`
/// while rejecting it for any real host — a non-loopback `http://`
/// registry/artifact/checksum fetch is a live MITM vector (an
/// on-path attacker can rewrite the manifest, the artifact bytes, or
/// the checksum it's checked against, in transit).
fn is_loopback_http_host(value: &str) -> bool {
  let Ok(parsed) = reqwest::Url::parse(value) else {
    return false;
  };
  match parsed.host_str() {
    Some("localhost") => true,
    Some(host) => host
      .parse::<std::net::IpAddr>()
      .map(|ip| ip.is_loopback())
      .unwrap_or(false),
    None => false,
  }
}

fn validate_http_url(field: &str, entry_name: &str, value: &str) -> Result<(), SkillError> {
  let value = value.trim();
  if value.is_empty() {
    return Err(validation_error(format!(
      "Marketplace entry '{}' source.{} must not be empty",
      entry_name, field
    )));
  }
  if value.starts_with("https://") {
    return Ok(());
  }
  if !value.starts_with("http://") {
    return Err(validation_error(format!(
      "Marketplace entry '{}' source.{} must be an http(s) URL",
      entry_name, field
    )));
  }
  if is_loopback_http_host(value) {
    return Ok(());
  }
  Err(validation_error(format!(
    "Marketplace entry '{}' source.{} uses a non-loopback http:// URL — only https:// or \
     loopback http:// (127.0.0.1 / ::1 / localhost) is allowed; a non-loopback http:// URL \
     is vulnerable to MITM rewriting of the manifest/artifact/checksum",
    entry_name, field
  )))
}

fn validate_registry_url(value: &str) -> Result<(), SkillError> {
  let value = value.trim();
  if value.is_empty() {
    return Err(validation_error(
      "Remote marketplace registry URL must not be empty".to_string(),
    ));
  }
  if value.starts_with("https://") {
    return Ok(());
  }
  if !value.starts_with("http://") {
    return Err(validation_error(
      "Remote marketplace registry URL must be an http(s) URL".to_string(),
    ));
  }
  if is_loopback_http_host(value) {
    return Ok(());
  }
  Err(validation_error(
    "Remote marketplace registry URL uses a non-loopback http:// URL — only https:// or \
     loopback http:// (127.0.0.1 / ::1 / localhost) is allowed; a non-loopback http:// URL \
     is vulnerable to MITM rewriting of the manifest/artifact/checksum"
      .to_string(),
  ))
}

fn normalize_sha256(value: &str) -> Result<String, SkillError> {
  let digest = value.trim().strip_prefix("sha256:").unwrap_or(value.trim());
  if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
    return Err(validation_error(format!(
      "Invalid artifact checksum '{}': expected sha256:<64 hex chars>",
      value
    )));
  }
  Ok(digest.to_ascii_lowercase())
}

fn sha256_bytes(bytes: &[u8]) -> String {
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  format!("{:x}", hasher.finalize())
}

fn sanitize_path_segment(value: &str) -> String {
  value
    .chars()
    .map(|ch| {
      if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
        ch
      } else {
        '_'
      }
    })
    .collect()
}

fn validation_error(message: String) -> SkillError {
  SkillError::ValidationError { message }
}

fn default_schema_version() -> u32 {
  DEFAULT_REMOTE_MARKETPLACE_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;
  use tempfile::TempDir;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::TcpListener;

  const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

  fn valid_manifest() -> RemoteMarketplaceManifest {
    RemoteMarketplaceManifest {
      schema_version: DEFAULT_REMOTE_MARKETPLACE_SCHEMA_VERSION,
      name: "yanshi-remote".into(),
      description: None,
      homepage: Some("https://example.com".into()),
      entries: vec![
        RemoteMarketplaceEntry {
          name: "rust-expert".into(),
          version: "1.2.3".into(),
          package_type: MarketplacePackageType::Skill,
          source: MarketplaceSource {
            registry_url: "https://registry.example.com/marketplace.toml".into(),
            artifact_url: "https://registry.example.com/skills/rust-expert.tar.gz".into(),
            checksum_sha256: format!("sha256:{DIGEST}"),
          },
          signature: Some(MarketplaceSignature {
            algorithm: "minisign".into(),
            key_id: "yanshi-dev".into(),
            value: "trusted-signature".into(),
          }),
          aliases: vec!["rust".into()],
          description: Some("Rust review skill".into()),
        },
        RemoteMarketplaceEntry {
          name: "echo-plugin".into(),
          version: "0.1.0".into(),
          package_type: MarketplacePackageType::Plugin,
          source: MarketplaceSource {
            registry_url: "https://registry.example.com/marketplace.toml".into(),
            artifact_url: "https://registry.example.com/plugins/echo-plugin.tar.gz".into(),
            checksum_sha256: DIGEST.to_string(),
          },
          signature: None,
          aliases: Vec::new(),
          description: None,
        },
      ],
    }
  }

  fn signed_entry_for_bytes(bytes: &[u8]) -> RemoteMarketplaceEntry {
    RemoteMarketplaceEntry {
      name: "rust/expert".into(),
      version: "1.0.0".into(),
      package_type: MarketplacePackageType::Skill,
      source: MarketplaceSource {
        registry_url: "https://registry.example.com/marketplace.toml".into(),
        artifact_url: "https://registry.example.com/rust-expert.tar.gz".into(),
        checksum_sha256: sha256_bytes(bytes),
      },
      signature: Some(MarketplaceSignature {
        algorithm: "checksum-sha256".into(),
        key_id: "dev".into(),
        value: sha256_bytes(bytes),
      }),
      aliases: Vec::new(),
      description: None,
    }
  }

  fn valid_manifest_toml(registry_url: &str) -> String {
    format!(
      r#"
schema_version = 1
name = "remote"

[[entries]]
name = "rust-expert"
version = "1.0.0"
type = "skill"
aliases = ["rust"]

[entries.source]
registry_url = "{registry_url}"
artifact_url = "https://registry.example.com/rust-expert.tar.gz"
checksum_sha256 = "sha256:{DIGEST}"
"#
    )
  }

  #[test]
  fn remote_marketplace_accepts_skill_and_plugin_entries() {
    let manifest = valid_manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.entries().len(), 2);
    assert_eq!(
      manifest.entries()[0].source.normalized_checksum().unwrap(),
      DIGEST
    );
  }

  #[test]
  fn remote_marketplace_loads_from_toml() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("marketplace.remote.toml");
    let mut file = fs::File::create(&path).unwrap();
    write!(
      file,
      r#"
schema_version = 1
name = "remote"

[[entries]]
name = "rust-expert"
version = "1.0.0"
type = "skill"
aliases = ["rust"]

[entries.source]
registry_url = "https://registry.example.com/marketplace.toml"
artifact_url = "https://registry.example.com/rust-expert.tar.gz"
checksum_sha256 = "sha256:{DIGEST}"

[entries.signature]
algorithm = "minisign"
key_id = "yanshi-dev"
value = "abc"
"#
    )
    .unwrap();

    let manifest = RemoteMarketplaceManifest::load(&path).unwrap();
    assert_eq!(manifest.name, "remote");
    assert_eq!(
      manifest.entries()[0].package_type,
      MarketplacePackageType::Skill
    );
  }

  #[test]
  fn remote_marketplace_rejects_duplicate_lookup_keys_per_type() {
    let mut manifest = valid_manifest();
    manifest.entries.push(RemoteMarketplaceEntry {
      name: "other".into(),
      version: "0.1.0".into(),
      package_type: MarketplacePackageType::Skill,
      source: MarketplaceSource {
        registry_url: "https://registry.example.com/marketplace.toml".into(),
        artifact_url: "https://registry.example.com/other.tar.gz".into(),
        checksum_sha256: DIGEST.to_string(),
      },
      signature: None,
      aliases: vec!["rust".into()],
      description: None,
    });

    let err = manifest.validate().unwrap_err();
    assert!(err.to_string().contains("Duplicate marketplace alias"));
  }

  #[test]
  fn remote_marketplace_allows_same_name_across_skill_and_plugin() {
    let mut manifest = valid_manifest();
    manifest.entries.push(RemoteMarketplaceEntry {
      name: "rust-expert".into(),
      version: "0.1.0".into(),
      package_type: MarketplacePackageType::Plugin,
      source: MarketplaceSource {
        registry_url: "https://registry.example.com/marketplace.toml".into(),
        artifact_url: "https://registry.example.com/rust-plugin.tar.gz".into(),
        checksum_sha256: DIGEST.to_string(),
      },
      signature: None,
      aliases: Vec::new(),
      description: None,
    });

    manifest.validate().unwrap();
  }

  #[test]
  fn remote_marketplace_rejects_invalid_checksum() {
    let mut manifest = valid_manifest();
    manifest.entries[0].source.checksum_sha256 = "sha256:not-a-digest".into();

    let err = manifest.validate().unwrap_err();
    assert!(err.to_string().contains("Invalid artifact checksum"));
  }

  #[test]
  fn remote_marketplace_rejects_non_http_sources() {
    let mut manifest = valid_manifest();
    manifest.entries[0].source.artifact_url = "file:///tmp/pkg.tar.gz".into();

    let err = manifest.validate().unwrap_err();
    assert!(err.to_string().contains("must be an http(s) URL"));
  }

  #[test]
  fn validate_http_url_rejects_non_loopback_http() {
    let mut manifest = valid_manifest();
    manifest.entries[0].source.artifact_url = "http://evil.example.com/pkg.tar.gz".into();

    let err = manifest.validate().unwrap_err();
    assert!(
      err.to_string().contains("non-loopback http:// URL"),
      "got: {err}"
    );
  }

  #[test]
  fn validate_http_url_accepts_loopback_http() {
    let mut manifest = valid_manifest();
    manifest.entries[0].source.artifact_url = "http://127.0.0.1:8080/pkg.tar.gz".into();
    manifest.entries[0].source.registry_url = "http://localhost:8080/marketplace.toml".into();

    assert!(manifest.validate().is_ok());
  }

  #[test]
  fn validate_registry_url_rejects_non_loopback_http() {
    let err = validate_registry_url("http://evil.example.com/marketplace.toml").unwrap_err();
    assert!(
      err.to_string().contains("non-loopback http:// URL"),
      "got: {err}"
    );
  }

  #[test]
  fn remote_marketplace_cache_new_defaults_to_ed25519_and_rejects_checksum_signature() {
    // V3.5: `RemoteMarketplaceCache::new()` must default to real
    // Ed25519 verification, not a self-computed checksum re-hash.
    // Proof: an entry signed with the (weaker) `checksum-sha256`
    // algorithm is rejected outright by the default constructor,
    // where it used to be silently accepted.
    let dir = TempDir::new().unwrap();
    let cache = RemoteMarketplaceCache::new(dir.path());
    let entry = signed_entry_for_bytes(b"expected");

    let err = cache.cache_artifact_bytes(&entry, b"expected").unwrap_err();
    assert!(
      err
        .to_string()
        .contains("Ed25519 verifier rejected algorithm"),
      "got: {err}"
    );
  }

  #[tokio::test]
  async fn remote_marketplace_client_fetches_read_only_manifest() {
    let (url, server) =
      spawn_registry_server(200, &valid_manifest_toml("http://127.0.0.1/index.toml")).await;
    let client = RemoteMarketplaceClient::new();

    let manifest = client.fetch_manifest(&url).await.unwrap();
    server.await.unwrap();

    assert_eq!(manifest.name, "remote");
    assert_eq!(manifest.entries().len(), 1);
    assert_eq!(manifest.entries()[0].name, "rust-expert");
  }

  #[tokio::test]
  async fn remote_marketplace_client_rejects_non_success_status() {
    let (url, server) = spawn_registry_server(404, "missing").await;
    let client = RemoteMarketplaceClient::new();

    let err = client.fetch_manifest(&url).await.unwrap_err();
    server.await.unwrap();

    assert!(err.to_string().contains("HTTP 404"));
  }

  #[tokio::test]
  async fn remote_marketplace_client_rejects_non_http_url() {
    let client = RemoteMarketplaceClient::new();
    let err = client
      .fetch_manifest("file:///tmp/marketplace.toml")
      .await
      .unwrap_err();
    assert!(err.to_string().contains("must be an http(s) URL"));
  }

  /// V3.5: `RemoteMarketplaceCache::new()` now defaults to
  /// `Ed25519SignatureVerifier` (see its constructor's doc comment),
  /// so tests that specifically exercise `ChecksumSha256SignatureVerifier`
  /// semantics (via `signed_entry_for_bytes`'s `checksum-sha256`-algorithm
  /// signature) must build their cache with that verifier explicitly
  /// rather than relying on the default.
  fn checksum_cache(root: impl Into<PathBuf>) -> RemoteMarketplaceCache {
    RemoteMarketplaceCache::with_client_and_verifier(
      root,
      RemoteMarketplaceClient::new(),
      Arc::new(ChecksumSha256SignatureVerifier),
    )
  }

  #[test]
  fn remote_marketplace_cache_writes_verified_artifact() {
    let dir = TempDir::new().unwrap();
    let cache = checksum_cache(dir.path());
    let bytes = b"package bytes";
    let entry = signed_entry_for_bytes(bytes);

    let cached = cache.cache_artifact_bytes(&entry, bytes).unwrap();

    assert!(cached.path.is_file());
    assert!(cached.signature_checked);
    assert_eq!(fs::read(cached.path).unwrap(), bytes);
    assert!(cache.is_cached(&entry).unwrap());
    assert!(
      cache
        .artifact_path(&entry)
        .unwrap()
        .to_string_lossy()
        .contains("rust_expert")
    );
  }

  #[test]
  fn remote_marketplace_cache_rejects_checksum_mismatch() {
    let dir = TempDir::new().unwrap();
    let cache = checksum_cache(dir.path());
    let mut entry = signed_entry_for_bytes(b"expected");
    entry.source.checksum_sha256 = sha256_bytes(b"different");

    let err = cache.cache_artifact_bytes(&entry, b"expected").unwrap_err();
    assert!(err.to_string().contains("Artifact checksum mismatch"));
  }

  #[test]
  fn remote_marketplace_cache_rejects_signature_mismatch() {
    let dir = TempDir::new().unwrap();
    let cache = checksum_cache(dir.path());
    let mut entry = signed_entry_for_bytes(b"expected");
    entry.signature.as_mut().unwrap().value = sha256_bytes(b"different");

    let err = cache.cache_artifact_bytes(&entry, b"expected").unwrap_err();
    assert!(err.to_string().contains("Signature checksum mismatch"));
  }

  #[test]
  fn remote_marketplace_cache_rejects_unsupported_signature_algorithm() {
    let dir = TempDir::new().unwrap();
    let cache = checksum_cache(dir.path());
    let mut entry = signed_entry_for_bytes(b"expected");
    entry.signature.as_mut().unwrap().algorithm = "minisign".into();

    let err = cache.cache_artifact_bytes(&entry, b"expected").unwrap_err();
    assert!(err.to_string().contains("Unsupported signature algorithm"));
  }

  // ── Q1.10.1: Ed25519 signature verifier ───────────────────────────────

  fn test_signing_key() -> ed25519_dalek::SigningKey {
    // Deterministic 32-byte seed — different from any real key.
    let seed: [u8; 32] = *b"yanshi-test-key-do-not-deploy!!!";
    ed25519_dalek::SigningKey::from_bytes(&seed)
  }

  fn write_test_pub_key(dir: &Path, key_id: &str, sk: &ed25519_dalek::SigningKey) {
    use base64::Engine;
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(format!("{}.pub", key_id)), pub_b64).unwrap();
  }

  #[test]
  fn ed25519_verifier_accepts_correctly_signed_artifact() {
    use base64::Engine;
    use ed25519_dalek::Signer;

    let key_dir = TempDir::new().unwrap();
    let sk = test_signing_key();
    write_test_pub_key(key_dir.path(), "publisher-a", &sk);

    let artifact = b"hello world";
    let sig = sk.sign(artifact);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    let entry = RemoteMarketplaceEntry {
      name: "skill-a".into(),
      version: "1.0.0".into(),
      package_type: MarketplacePackageType::Skill,
      source: MarketplaceSource {
        registry_url: "https://example.test/registry".into(),
        artifact_url: "https://example.test/skill-a.tar.gz".into(),
        checksum_sha256: sha256_bytes(artifact),
      },
      signature: Some(MarketplaceSignature {
        algorithm: "ed25519".into(),
        key_id: "publisher-a".into(),
        value: sig_b64,
      }),
      aliases: vec![],
      description: None,
    };

    let verifier = Ed25519SignatureVerifier::new(key_dir.path());
    verifier
      .verify(&entry, artifact)
      .expect("verification must pass for legit sig");
  }

  #[test]
  fn ed25519_verifier_rejects_tampered_artifact() {
    use base64::Engine;
    use ed25519_dalek::Signer;

    let key_dir = TempDir::new().unwrap();
    let sk = test_signing_key();
    write_test_pub_key(key_dir.path(), "publisher-a", &sk);

    let original = b"hello world";
    let sig = sk.sign(original);
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    let tampered = b"goodbye world"; // same length, different content
    let entry = RemoteMarketplaceEntry {
      name: "skill-a".into(),
      version: "1.0.0".into(),
      package_type: MarketplacePackageType::Skill,
      source: MarketplaceSource {
        registry_url: "https://example.test/registry".into(),
        artifact_url: "https://example.test/skill-a.tar.gz".into(),
        checksum_sha256: sha256_bytes(tampered),
      },
      signature: Some(MarketplaceSignature {
        algorithm: "ed25519".into(),
        key_id: "publisher-a".into(),
        value: sig_b64,
      }),
      aliases: vec![],
      description: None,
    };

    let verifier = Ed25519SignatureVerifier::new(key_dir.path());
    let err = verifier.verify(&entry, tampered).unwrap_err();
    assert!(
      err.to_string().contains("verification failed"),
      "expected verification failure, got: {err}"
    );
  }

  #[test]
  fn ed25519_verifier_rejects_unsigned_entry_when_required() {
    let key_dir = TempDir::new().unwrap();
    let mut entry = signed_entry_for_bytes(b"data");
    entry.signature = None;

    let verifier = Ed25519SignatureVerifier::new(key_dir.path());
    let err = verifier.verify(&entry, b"data").unwrap_err();
    assert!(err.to_string().contains("requires one"));
  }

  #[test]
  fn ed25519_verifier_rejects_path_traversal_in_key_id() {
    let key_dir = TempDir::new().unwrap();
    let entry = RemoteMarketplaceEntry {
      name: "skill-a".into(),
      version: "1.0.0".into(),
      package_type: MarketplacePackageType::Skill,
      source: MarketplaceSource {
        registry_url: "https://example.test/registry".into(),
        artifact_url: "https://example.test/skill-a.tar.gz".into(),
        checksum_sha256: sha256_bytes(b"data"),
      },
      signature: Some(MarketplaceSignature {
        algorithm: "ed25519".into(),
        key_id: "../etc/passwd".into(),
        value: "AAAA".into(),
      }),
      aliases: vec![],
      description: None,
    };

    let verifier = Ed25519SignatureVerifier::new(key_dir.path());
    let err = verifier.verify(&entry, b"data").unwrap_err();
    assert!(err.to_string().contains("not a valid filename component"));
  }

  #[test]
  fn remote_marketplace_cache_verifies_existing_artifact() {
    let dir = TempDir::new().unwrap();
    let cache = checksum_cache(dir.path());
    let bytes = b"package bytes";
    let entry = signed_entry_for_bytes(bytes);
    cache.cache_artifact_bytes(&entry, bytes).unwrap();

    let cached = cache.verify_cached_artifact(&entry).unwrap();

    assert_eq!(cached.checksum_sha256, sha256_bytes(bytes));
    assert!(cached.signature_checked);
  }

  async fn spawn_registry_server(status: u16, body: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    let handle = tokio::spawn(async move {
      let (mut socket, _) = listener.accept().await.unwrap();
      let mut request = vec![0u8; 4096];
      let _ = socket.read(&mut request).await.unwrap();
      let reason = if status == 200 { "OK" } else { "Not Found" };
      let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      socket.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{addr}/marketplace.toml"), handle)
  }
}
