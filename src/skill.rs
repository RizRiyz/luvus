//! Explicit, remotely distributed agent skills.
//!
//! Luvus never installs or refreshes a skill during startup. A user opts one
//! agent in with `luvus skill enable <agent>`, after which `skill update` may
//! replace only that managed installation. Production skill instructions live
//! outside this repository and are authenticated by a signed manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use fs2::FileExt;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_MANIFEST_URL: &str = "https://luvus.dev/skills/manifest.json";
const STATE_SCHEMA: u32 = 1;
const MANIFEST_SCHEMA: u32 = 1;
const PACKAGE_SCHEMA: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PACKAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_FETCH_ERROR_BYTES: usize = 64 * 1024;
const MIGRATION_MARKER: &str = ".migrated-opt-in-v1";

const POINTER_BEGIN: &str = "<!-- BEGIN luvus (managed by luvus; do not edit inside) -->";
const POINTER_END: &str = "<!-- END luvus -->";
const LEGACY_POINTER_BEGIN: &str = "<!-- BEGIN bohay (managed by bohay; do not edit inside) -->";
const LEGACY_POINTER_END: &str = "<!-- END bohay -->";

// Exact known auto-installed files from the last bundled Bohay and Luvus
// releases. Hashes let migration remove only untouched managed files without
// retaining their instruction bodies in this binary.
const KNOWN_SKILL_HASHES: &[&str] = &[
    "8238018819b13f792a1f76eba8991b8c4ac8b89423312e2d53342decc1cfce38",
    "9336188a0d6d59afcc6a111eb4b89951efca3f7afa8fbb534b2bf416292f9c5a",
];
const KNOWN_REFERENCE_HASHES: &[&str] = &[
    "106a11df5a030b9f4f425b6576eb7de8a4b386597bf43e20f4aeb174c1ba2343",
    "708167a648407f41fda30c66510fb6bca5dc20a544c0b2aa4e14623427ed055e",
];

static INSTALL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillAgent {
    Claude,
    Codex,
    Opencode,
}

impl SkillAgent {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Opencode];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

impl fmt::Display for SkillAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SkillAgent {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::Opencode),
            _ => bail!("unknown skill agent `{value}`; expected claude, codex, or opencode"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledSkill {
    pub release: String,
    pub source: String,
    pub target: PathBuf,
    pub files: Vec<ManagedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillState {
    #[serde(default = "state_schema")]
    schema: u32,
    #[serde(default)]
    agents: BTreeMap<String, InstalledSkill>,
}

impl Default for SkillState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            agents: BTreeMap::new(),
        }
    }
}

fn state_schema() -> u32 {
    STATE_SCHEMA
}

#[derive(Debug, Clone, Deserialize)]
struct SignedEnvelope {
    signed: String,
    signature: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillManifest {
    schema: u32,
    release: String,
    requires_luvus: String,
    artifacts: BTreeMap<String, SkillArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillArtifact {
    url: String,
    sha256: String,
    size: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillPackage {
    schema: u32,
    agent: SkillAgent,
    release: String,
    files: Vec<PackageFile>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackageFile {
    path: String,
    content: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct VerifiedPackage {
    agent: SkillAgent,
    release: String,
    files: Vec<VerifiedFile>,
}

#[derive(Debug, Clone)]
struct VerifiedFile {
    path: PathBuf,
    content: Vec<u8>,
    sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    Current,
    Missing,
    Modified,
}

impl fmt::Display for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Modified => "modified",
        })
    }
}

#[derive(Debug, Clone)]
pub struct SkillStatus {
    pub agent: SkillAgent,
    pub installed: Option<InstalledSkill>,
    pub integrity: Option<Integrity>,
}

fn state_path() -> PathBuf {
    crate::persist::skills_dir().join("state.json")
}

fn state_lock_path() -> PathBuf {
    crate::persist::skills_dir().join("state.lock")
}

fn migration_marker_path() -> PathBuf {
    crate::persist::skills_dir().join(MIGRATION_MARKER)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let existed = path.is_dir();
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protecting {}", path.display()))?;
    }
    Ok(())
}

fn lock_state() -> Result<File> {
    let dir = crate::persist::skills_dir();
    ensure_private_dir(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(state_lock_path())
        .context("opening skill state lock")?;
    file.lock_exclusive().context("locking skill state")?;
    Ok(file)
}

fn load_state() -> Result<SkillState> {
    let path = state_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SkillState::default());
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let state: SkillState =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if state.schema > STATE_SCHEMA {
        bail!(
            "skill state schema {} is newer than this Luvus supports ({STATE_SCHEMA})",
            state.schema
        );
    }
    Ok(state)
}

fn save_state(state: &SkillState) -> Result<()> {
    let dir = crate::persist::skills_dir();
    ensure_private_dir(&dir)?;
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).context("serializing skill state")?;
    let mut file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {}", tmp.display()))?;
    atomic_replace_file(&tmp, &path)
}

fn atomic_replace_file(tmp: &Path, path: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    }
    #[cfg(windows)]
    {
        let backup = path.with_extension("json.previous");
        let _ = fs::remove_file(&backup);
        if path.exists() {
            fs::rename(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
        }
        if let Err(err) = fs::rename(tmp, path) {
            let _ = fs::rename(&backup, path);
            return Err(err).with_context(|| format!("replacing {}", path.display()));
        }
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn target_dir_at(agent: SkillAgent, home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    match agent {
        SkillAgent::Claude => home.join(".claude").join("skills").join("luvus"),
        SkillAgent::Codex => home.join(".agents").join("skills").join("luvus"),
        SkillAgent::Opencode => xdg_config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode")
            .join("skills")
            .join("luvus"),
    }
}

fn target_dir(agent: SkillAgent) -> Result<PathBuf> {
    let home = crate::platform::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(target_dir_at(agent, &home, xdg.as_deref()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

fn decode_b64<const N: usize>(label: &str, encoded: &str) -> Result<[u8; N]> {
    let bytes = BASE64
        .decode(encoded.trim())
        .with_context(|| format!("decoding {label}"))?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| anyhow!("{label} has {} bytes; expected {N}", bytes.len()))
}

fn verification_key() -> Result<VerifyingKey> {
    let encoded = option_env!("LUVUS_SKILL_PUBLIC_KEY_B64")
        .map(str::to_owned)
        .or_else(|| {
            cfg!(debug_assertions)
                .then(|| std::env::var("LUVUS_SKILL_PUBLIC_KEY_B64").ok())
                .flatten()
        })
        .ok_or_else(|| {
            anyhow!(
                "this Luvus build has no skill verification key; release builds must set LUVUS_SKILL_PUBLIC_KEY_B64"
            )
        })?;
    VerifyingKey::from_bytes(&decode_b64("skill verification key", &encoded)?)
        .context("invalid skill verification key")
}

fn parse_signed_manifest(bytes: &[u8], key: &VerifyingKey) -> Result<SkillManifest> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("skill manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit");
    }
    let envelope: SignedEnvelope =
        serde_json::from_slice(bytes).context("parsing signed skill manifest envelope")?;
    let signed = BASE64
        .decode(envelope.signed.trim())
        .context("decoding signed skill manifest")?;
    if signed.len() > MAX_MANIFEST_BYTES {
        bail!("signed skill manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit");
    }
    let signature = Signature::from_slice(
        &BASE64
            .decode(envelope.signature.trim())
            .context("decoding skill manifest signature")?,
    )
    .context("invalid skill manifest signature shape")?;
    key.verify(&signed, &signature)
        .context("skill manifest signature verification failed")?;
    let manifest: SkillManifest =
        serde_json::from_slice(&signed).context("parsing verified skill manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &SkillManifest) -> Result<()> {
    if manifest.schema != MANIFEST_SCHEMA {
        bail!(
            "unsupported skill manifest schema {}; expected {MANIFEST_SCHEMA}",
            manifest.schema
        );
    }
    if manifest.release.trim().is_empty() {
        bail!("skill manifest release is empty");
    }
    let requirement = VersionReq::parse(&manifest.requires_luvus)
        .context("invalid skill manifest Luvus version requirement")?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("invalid compiled Luvus package version")?;
    if !requirement.matches(&current) {
        bail!(
            "skill release {} requires Luvus {}; current Luvus is {}",
            manifest.release,
            manifest.requires_luvus,
            current
        );
    }
    for (agent, artifact) in &manifest.artifacts {
        if agent.parse::<SkillAgent>().is_err() {
            continue;
        }
        if artifact.size == 0 || artifact.size > MAX_PACKAGE_BYTES {
            bail!("{agent} skill artifact has invalid size {}", artifact.size);
        }
        validate_sha256(&artifact.sha256)?;
        validate_remote_url(&artifact.url)?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest `{value}`");
    }
    Ok(())
}

fn validate_remote_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if cfg!(debug_assertions)
        && std::env::var_os("LUVUS_SKILL_ALLOW_FILE").as_deref() == Some(std::ffi::OsStr::new("1"))
        && url.starts_with("file://")
    {
        return Ok(());
    }
    bail!(
        "skill sources must use HTTPS (file:// requires LUVUS_SKILL_ALLOW_FILE=1 in debug builds)"
    )
}

fn fetch_bytes(url: &str, limit: usize) -> Result<Vec<u8>> {
    validate_remote_url(url)?;
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = fs::read(path).with_context(|| format!("reading {path}"))?;
        if bytes.len() > limit {
            bail!("download from {url} exceeds the {limit}-byte limit");
        }
        return Ok(bytes);
    }

    let max = limit.to_string();
    let curl = [
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--max-time",
        "20",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-filesize",
        max.as_str(),
        "--header",
        "Accept: application/json",
        "--header",
        "User-Agent: luvus",
        url,
    ];
    if let Some(bytes) = try_fetch_command("curl", &curl, limit)? {
        return Ok(bytes);
    }

    let quota = format!("--quota={limit}");
    let wget = [
        "-q",
        "-O",
        "-",
        "--timeout=20",
        "--tries=1",
        "--https-only",
        quota.as_str(),
        "--header=Accept: application/json",
        "--header=User-Agent: luvus",
        url,
    ];
    if let Some(bytes) = try_fetch_command("wget", &wget, limit)? {
        return Ok(bytes);
    }
    bail!("need curl or wget to download Luvus skills")
}

fn try_fetch_command(prog: &str, args: &[&str], limit: usize) -> Result<Option<Vec<u8>>> {
    let mut child = match crate::platform::no_window(
        Command::new(prog)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .spawn()
    {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("running {prog}")),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("capturing {prog} output"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("capturing {prog} errors"))?;
    let errors = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let keep = read.min(MAX_FETCH_ERROR_BYTES.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&chunk[..keep]);
                    truncated |= keep < read;
                }
            }
        }
        (bytes, truncated)
    });

    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let read = stdout.take((limit + 1) as u64).read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > limit {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .with_context(|| format!("waiting for {prog}"))?;
    let (stderr, stderr_truncated) = errors.join().unwrap_or_default();
    read.with_context(|| format!("reading {prog} response"))?;
    if bytes.len() > limit {
        bail!("{prog} response exceeds the {limit}-byte limit");
    }
    if !status.success() {
        let mut message = String::from_utf8_lossy(&stderr).trim().to_string();
        if stderr_truncated {
            message.push('…');
        }
        bail!("{prog}: {message}");
    }
    Ok(Some(bytes))
}

fn fetch_manifest(url: &str) -> Result<SkillManifest> {
    let key = verification_key()?;
    let bytes = fetch_bytes(url, MAX_MANIFEST_BYTES)?;
    parse_signed_manifest(&bytes, &key)
}

fn fetch_package(manifest: &SkillManifest, agent: SkillAgent) -> Result<VerifiedPackage> {
    let artifact = manifest
        .artifacts
        .get(agent.as_str())
        .ok_or_else(|| anyhow!("skill release {} has no {agent} artifact", manifest.release))?;
    let bytes = fetch_bytes(&artifact.url, artifact.size.min(MAX_PACKAGE_BYTES))?;
    if bytes.len() != artifact.size {
        bail!(
            "{agent} skill artifact size mismatch: got {}, expected {}",
            bytes.len(),
            artifact.size
        );
    }
    if sha256_hex(&bytes) != artifact.sha256.to_ascii_lowercase() {
        bail!("{agent} skill artifact SHA-256 mismatch");
    }
    let package: SkillPackage =
        serde_json::from_slice(&bytes).context("parsing verified skill package")?;
    validate_package(package, manifest, agent)
}

fn validate_package(
    package: SkillPackage,
    manifest: &SkillManifest,
    agent: SkillAgent,
) -> Result<VerifiedPackage> {
    if package.schema != PACKAGE_SCHEMA {
        bail!(
            "unsupported skill package schema {}; expected {PACKAGE_SCHEMA}",
            package.schema
        );
    }
    if package.agent != agent {
        bail!(
            "skill package targets {}, not {agent}",
            package.agent.as_str()
        );
    }
    if package.release != manifest.release {
        bail!(
            "skill package release {} does not match manifest {}",
            package.release,
            manifest.release
        );
    }
    if package.files.is_empty() {
        bail!("skill package contains no files");
    }

    let mut seen = BTreeSet::new();
    let mut verified = Vec::with_capacity(package.files.len());
    let mut has_skill = false;
    let mut total = 0usize;
    for file in package.files {
        validate_sha256(&file.sha256)?;
        let path = safe_package_path(&file.path)?;
        if !seen.insert(path.clone()) {
            bail!("duplicate skill package path {}", path.display());
        }
        let content = file.content.into_bytes();
        if content.len() > MAX_FILE_BYTES {
            bail!("skill file {} exceeds the size limit", path.display());
        }
        total = total
            .checked_add(content.len())
            .ok_or_else(|| anyhow!("skill package size overflow"))?;
        if total > MAX_PACKAGE_BYTES {
            bail!("expanded skill package exceeds the size limit");
        }
        let digest = sha256_hex(&content);
        if digest != file.sha256.to_ascii_lowercase() {
            bail!("skill file {} SHA-256 mismatch", path.display());
        }
        if path == Path::new("SKILL.md") {
            validate_skill_text(&content)?;
            has_skill = true;
        }
        verified.push(VerifiedFile {
            path,
            content,
            sha256: digest,
        });
    }
    if !has_skill {
        bail!("skill package is missing SKILL.md");
    }
    Ok(VerifiedPackage {
        agent,
        release: package.release,
        files: verified,
    })
}

fn safe_package_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if value.is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("unsafe skill package path `{value}`");
    }
    let allowed =
        value == "SKILL.md" || value == "agents/openai.yaml" || value.starts_with("references/");
    if !allowed {
        bail!("unsupported skill package path `{value}`");
    }
    Ok(path.to_path_buf())
}

fn validate_skill_text(bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes).context("SKILL.md is not UTF-8")?;
    let trimmed = text.trim_start();
    if !(400..=MAX_FILE_BYTES).contains(&bytes.len())
        || !trimmed.starts_with("---")
        || !trimmed.contains("name: luvus")
        || !trimmed.contains("description:")
        || !trimmed.contains("=target")
        || !trimmed.contains("agent send")
    {
        bail!("SKILL.md failed Luvus skill validation");
    }
    Ok(())
}

fn collect_relative_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                bail!(
                    "managed skill contains a symlink: {}",
                    entry.path().display()
                );
            }
            if kind.is_dir() {
                walk(root, &entry.path(), out)?;
            } else if kind.is_file() {
                out.insert(entry.path().strip_prefix(root)?.to_path_buf());
            } else {
                bail!("managed skill contains an unsupported file type");
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    if root.is_dir() {
        walk(root, root, &mut files)?;
    }
    Ok(files)
}

fn integrity(record: &InstalledSkill) -> Result<Integrity> {
    if !record.target.exists() {
        return Ok(Integrity::Missing);
    }
    if !record.target.is_dir() {
        return Ok(Integrity::Modified);
    }
    let expected: BTreeSet<PathBuf> = record
        .files
        .iter()
        .map(|file| PathBuf::from(&file.path))
        .collect();
    if collect_relative_files(&record.target)? != expected {
        return Ok(Integrity::Modified);
    }
    for file in &record.files {
        if hash_file(&record.target.join(&file.path))? != file.sha256 {
            return Ok(Integrity::Modified);
        }
    }
    Ok(Integrity::Current)
}

fn stage_package(target: &Path, package: &VerifiedPackage) -> Result<(PathBuf, Vec<ManagedFile>)> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("skill target has no parent"))?;
    ensure_private_dir(parent)?;
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stage = parent.join(format!(
        ".luvus-skill-stage-{}-{}-{sequence}",
        package.agent,
        std::process::id()
    ));
    if stage.exists() {
        fs::remove_dir_all(&stage)
            .with_context(|| format!("removing stale stage {}", stage.display()))?;
    }
    ensure_private_dir(&stage)?;

    let result = (|| {
        let mut managed = Vec::with_capacity(package.files.len());
        for file in &package.files {
            let path = stage.join(&file.path);
            if let Some(dir) = path.parent() {
                ensure_private_dir(dir)?;
            }
            let mut output = File::create(&path)
                .with_context(|| format!("creating staged skill file {}", path.display()))?;
            output
                .write_all(&file.content)
                .with_context(|| format!("writing staged skill file {}", path.display()))?;
            output
                .sync_all()
                .with_context(|| format!("flushing staged skill file {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
            managed.push(ManagedFile {
                path: file.path.to_string_lossy().into_owned(),
                sha256: file.sha256.clone(),
            });
        }
        Ok(managed)
    })();
    match result {
        Ok(managed) => Ok((stage, managed)),
        Err(err) => {
            let _ = fs::remove_dir_all(&stage);
            Err(err)
        }
    }
}

fn install_package(
    state: &mut SkillState,
    source: &str,
    package: &VerifiedPackage,
) -> Result<InstalledSkill> {
    let target = target_dir(package.agent)?;
    install_package_at(state, source, package, target)
}

fn install_package_at(
    state: &mut SkillState,
    source: &str,
    package: &VerifiedPackage,
    target: PathBuf,
) -> Result<InstalledSkill> {
    let key = package.agent.as_str();
    let previous = state.agents.get(key).cloned();
    match previous.as_ref() {
        Some(record) => {
            if record.target != target {
                bail!(
                    "managed {key} skill target changed from {} to {}; disable it first",
                    record.target.display(),
                    target.display()
                );
            }
            if integrity(record)? != Integrity::Current {
                bail!("managed {key} skill was modified or is incomplete; disable or repair it before updating");
            }
        }
        None if target.exists() => {
            bail!(
                "{} already exists but is not owned by Luvus; move it or choose not to enable {key}",
                target.display()
            );
        }
        None => {}
    }

    let (stage, files) = stage_package(&target, package)?;
    let parent = target.parent().expect("target parent checked");
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let backup = parent.join(format!(
        ".luvus-skill-backup-{}-{}-{sequence}",
        package.agent,
        std::process::id()
    ));
    if target.exists() {
        fs::rename(&target, &backup).with_context(|| format!("backing up {}", target.display()))?;
    }
    if let Err(err) = fs::rename(&stage, &target) {
        let _ = fs::rename(&backup, &target);
        let _ = fs::remove_dir_all(&stage);
        return Err(err).with_context(|| format!("installing {} skill", package.agent));
    }

    let installed = InstalledSkill {
        release: package.release.clone(),
        source: source.to_string(),
        target: target.clone(),
        files,
    };
    state.agents.insert(key.to_string(), installed.clone());
    if let Err(err) = save_state(state) {
        let _ = fs::remove_dir_all(&target);
        if backup.exists() {
            let _ = fs::rename(&backup, &target);
        }
        match previous {
            Some(record) => {
                state.agents.insert(key.to_string(), record);
            }
            None => {
                state.agents.remove(key);
            }
        }
        return Err(err).context("saving skill ownership; installation rolled back");
    }
    if backup.exists() {
        // The new tree and ownership state are already committed. Leaving a
        // stale backup is preferable to reporting that a successful update
        // failed and encouraging an unsafe retry.
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(installed)
}

pub fn enable(agents: &[SkillAgent], manifest_url: &str) -> Result<Vec<InstalledSkill>> {
    if agents.is_empty() {
        bail!("skill enable requires an agent or --all");
    }
    let unique: BTreeSet<_> = agents.iter().copied().collect();
    let agents: Vec<_> = unique.into_iter().collect();
    validate_remote_url(manifest_url)?;
    let manifest = fetch_manifest(manifest_url)?;
    let lock = lock_state()?;
    let mut state = load_state()?;
    let mut installed = Vec::with_capacity(agents.len());
    let mut pending = Vec::new();
    for agent in agents {
        let existing = state.agents.get(agent.as_str());
        if existing.is_some_and(|record| {
            record.release == manifest.release && integrity(record).ok() == Some(Integrity::Current)
        }) {
            installed.push(existing.cloned().expect("checked above"));
            continue;
        }
        pending.push(agent);
    }
    let mut packages = BTreeMap::new();
    for agent in &pending {
        packages.insert(*agent, fetch_package(&manifest, *agent)?);
    }
    for agent in pending {
        installed.push(install_package(
            &mut state,
            manifest_url,
            packages.get(&agent).expect("loaded for every agent"),
        )?);
    }
    FileExt::unlock(&lock).ok();
    Ok(installed)
}

pub fn update(agent: Option<SkillAgent>, manifest_url: &str) -> Result<Vec<InstalledSkill>> {
    let state = load_state()?;
    let agents: Vec<SkillAgent> = match agent {
        Some(agent) => {
            if !state.agents.contains_key(agent.as_str()) {
                bail!("{agent} skill is disabled; run `luvus skill enable {agent}` first");
            }
            vec![agent]
        }
        None => SkillAgent::ALL
            .into_iter()
            .filter(|agent| state.agents.contains_key(agent.as_str()))
            .collect(),
    };
    if agents.is_empty() {
        return Ok(Vec::new());
    }
    enable(&agents, manifest_url)
}

pub fn disable(agent: SkillAgent) -> Result<Option<PathBuf>> {
    let lock = lock_state()?;
    let mut state = load_state()?;
    let Some(record) = state.agents.get(agent.as_str()).cloned() else {
        FileExt::unlock(&lock).ok();
        return Ok(None);
    };
    if record.target.exists() && integrity(&record)? != Integrity::Current {
        bail!(
            "managed {agent} skill at {} was modified; it was preserved",
            record.target.display()
        );
    }

    let parent = record
        .target
        .parent()
        .ok_or_else(|| anyhow!("managed skill target has no parent"))?;
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let backup = parent.join(format!(
        ".luvus-skill-disable-{agent}-{}-{sequence}",
        std::process::id()
    ));
    if record.target.exists() {
        fs::rename(&record.target, &backup)
            .with_context(|| format!("staging removal of {}", record.target.display()))?;
    }
    state.agents.remove(agent.as_str());
    if let Err(err) = save_state(&state) {
        if backup.exists() {
            let _ = fs::rename(&backup, &record.target);
        }
        return Err(err).context("saving disabled skill state; removal rolled back");
    }
    if backup.exists() {
        // The target is disabled in state already; cleanup failure must not
        // turn that committed result into a misleading command failure.
        let _ = fs::remove_dir_all(&backup);
    }
    FileExt::unlock(&lock).ok();
    Ok(Some(record.target))
}

pub fn status(agent: SkillAgent) -> Result<SkillStatus> {
    let state = load_state()?;
    let installed = state.agents.get(agent.as_str()).cloned();
    let integrity = installed
        .as_ref()
        .map(super::skill::integrity)
        .transpose()?;
    Ok(SkillStatus {
        agent,
        installed,
        integrity,
    })
}

pub fn statuses() -> Result<Vec<SkillStatus>> {
    SkillAgent::ALL.into_iter().map(status).collect()
}

pub fn show(agent: SkillAgent) -> Result<String> {
    let status = status(agent)?;
    let installed = status
        .installed
        .ok_or_else(|| anyhow!("{agent} skill is disabled"))?;
    fs::read_to_string(installed.target.join("SKILL.md"))
        .with_context(|| format!("reading installed {agent} SKILL.md"))
}

fn strip_all_blocks(text: &str, begin: &str, end_marker: &str) -> String {
    let mut output = text.to_string();
    loop {
        let Some(begin_at) = output.find(begin) else {
            return output;
        };
        let after_begin = begin_at + begin.len();
        let Some(end_offset) = output[after_begin..].find(end_marker) else {
            return output;
        };
        let end = after_begin + end_offset + end_marker.len();
        output.replace_range(begin_at..end, "");
    }
}

fn strip_managed_blocks(text: &str) -> String {
    let current = strip_all_blocks(text, POINTER_BEGIN, POINTER_END);
    strip_all_blocks(&current, LEGACY_POINTER_BEGIN, LEGACY_POINTER_END)
}

fn remove_managed_blocks(path: &Path) -> Result<bool> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let cleaned = strip_managed_blocks(&text);
    if cleaned == text {
        return Ok(false);
    }
    fs::write(path, cleaned).with_context(|| format!("updating {}", path.display()))?;
    Ok(true)
}

fn remove_known_file(path: &Path, hashes: &[&str]) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    if !hashes.contains(&hash_file(path)?.as_str()) {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    Ok(true)
}

fn remove_known_legacy_skill(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let skill = dir.join("SKILL.md");
    if remove_known_file(&skill, KNOWN_SKILL_HASHES)? {
        removed.push(skill);
    }
    let reference = dir.join("references").join("advanced-control.md");
    if remove_known_file(&reference, KNOWN_REFERENCE_HASHES)? {
        removed.push(reference);
    }
    let _ = fs::remove_dir(dir.join("references"));
    let _ = fs::remove_dir(dir);
    Ok(removed)
}

fn migrate_legacy_at(
    home: &Path,
    xdg_config: Option<&Path>,
    codex_home: Option<&Path>,
    marker: &Path,
) -> Result<Vec<PathBuf>> {
    if marker.is_file() {
        return Ok(Vec::new());
    }
    let mut changed = Vec::new();
    let codex_agents = codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"))
        .join("AGENTS.md");
    let opencode_agents = xdg_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
        .join("opencode")
        .join("AGENTS.md");
    for path in [codex_agents, opencode_agents] {
        if remove_managed_blocks(&path)? {
            changed.push(path);
        }
    }
    let claude_skills = home.join(".claude").join("skills");
    changed.extend(remove_known_legacy_skill(&claude_skills.join("luvus"))?);
    changed.extend(remove_known_legacy_skill(&claude_skills.join("bohay"))?);

    if let Some(parent) = marker.parent() {
        ensure_private_dir(parent)?;
    }
    fs::write(marker, b"opt-in skill migration complete\n")
        .with_context(|| format!("writing {}", marker.display()))?;
    Ok(changed)
}

/// One-time cleanup for the former default-on installer. Debug builds skip
/// automatic external-agent edits so development never touches the user's
/// production agent configuration. Release builds still migrate users who
/// intentionally configure a custom Luvus state directory. Tests exercise
/// `migrate_legacy_at` with explicit isolated paths.
pub fn migrate_legacy_installation() -> Result<Vec<PathBuf>> {
    if cfg!(debug_assertions) {
        return Ok(Vec::new());
    }
    let home = crate::platform::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    migrate_legacy_at(
        &home,
        xdg.as_deref(),
        codex.as_deref(),
        &migration_marker_path(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn valid_skill() -> String {
        format!(
            "---\nname: luvus\ndescription: Control Luvus only when explicitly requested.\n---\n\n# Luvus\n\n=target uses luvus agent send.\n{}",
            "Detailed safe control instructions. ".repeat(16)
        )
    }

    fn package(agent: SkillAgent, release: &str) -> SkillPackage {
        let skill = valid_skill();
        SkillPackage {
            schema: PACKAGE_SCHEMA,
            agent,
            release: release.to_string(),
            files: vec![PackageFile {
                path: "SKILL.md".into(),
                sha256: sha256_hex(skill.as_bytes()),
                content: skill,
            }],
        }
    }

    fn manifest(release: &str) -> SkillManifest {
        SkillManifest {
            schema: MANIFEST_SCHEMA,
            release: release.to_string(),
            requires_luvus: ">=0.11.0, <0.12.0".into(),
            artifacts: BTreeMap::new(),
        }
    }

    #[test]
    fn signed_manifest_requires_the_expected_key() {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let other = SigningKey::from_bytes(&[8; 32]);
        let signed = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "release": "0.4.0",
            "requires_luvus": ">=0.11.0, <0.12.0",
            "artifacts": {}
        }))
        .unwrap();
        let envelope = serde_json::to_vec(&serde_json::json!({
            "signed": BASE64.encode(&signed),
            "signature": BASE64.encode(signing.sign(&signed).to_bytes())
        }))
        .unwrap();
        let parsed = parse_signed_manifest(&envelope, &signing.verifying_key()).unwrap();
        assert_eq!(parsed.release, "0.4.0");
        assert!(parse_signed_manifest(&envelope, &other.verifying_key()).is_err());
    }

    #[test]
    fn manifest_ignores_artifacts_for_future_agents() {
        let mut manifest = manifest("0.4.0");
        manifest.artifacts.insert(
            "future-agent".into(),
            SkillArtifact {
                url: "not-used".into(),
                sha256: "not-used".into(),
                size: 0,
            },
        );
        validate_manifest(&manifest).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn command_fetch_is_bounded_while_reading() {
        let error = try_fetch_command("sh", &["-c", "printf 12345678901"], 10)
            .unwrap_err()
            .to_string();
        assert!(error.contains("10-byte limit"), "{error}");
    }

    #[test]
    fn package_validation_rejects_traversal_and_hash_mismatch() {
        let manifest = manifest("0.4.0");
        let mut bad_path = package(SkillAgent::Codex, "0.4.0");
        bad_path.files[0].path = "../SKILL.md".into();
        assert!(validate_package(bad_path, &manifest, SkillAgent::Codex).is_err());

        let mut bad_hash = package(SkillAgent::Codex, "0.4.0");
        bad_hash.files[0].sha256 = "0".repeat(64);
        assert!(validate_package(bad_hash, &manifest, SkillAgent::Codex).is_err());
    }

    #[test]
    fn native_targets_do_not_use_agents_md() {
        let home = Path::new("/home/tester");
        assert_eq!(
            target_dir_at(SkillAgent::Codex, home, None),
            PathBuf::from("/home/tester/.agents/skills/luvus")
        );
        assert!(!target_dir_at(SkillAgent::Codex, home, None).ends_with("AGENTS.md"));
        assert!(!target_dir_at(SkillAgent::Opencode, home, None).ends_with("AGENTS.md"));
        assert_eq!(
            target_dir_at(SkillAgent::Opencode, home, Some(Path::new("/xdg/config"))),
            PathBuf::from("/xdg/config/opencode/skills/luvus")
        );
    }

    #[test]
    fn managed_install_and_disable_preserve_modified_files() {
        let _env = crate::persist::test_env("skill-managed-install");
        let root = crate::persist::skills_dir().join("agent-home");
        let target = root.join(".agents/skills/luvus");
        let manifest = manifest("0.4.0");
        let package = validate_package(
            package(SkillAgent::Codex, "0.4.0"),
            &manifest,
            SkillAgent::Codex,
        )
        .unwrap();
        let mut state = SkillState::default();
        let installed = install_package_at(
            &mut state,
            "https://luvus.dev/skills/manifest.json",
            &package,
            target.clone(),
        )
        .unwrap();
        assert_eq!(installed.target, target);
        assert_eq!(integrity(&installed).unwrap(), Integrity::Current);

        fs::write(target.join("SKILL.md"), "user changed this skill").unwrap();
        let err = disable(SkillAgent::Codex).unwrap_err().to_string();
        assert!(err.contains("modified"), "{err}");
        assert!(target.join("SKILL.md").is_file());

        fs::write(target.join("SKILL.md"), &package.files[0].content).unwrap();
        assert_eq!(disable(SkillAgent::Codex).unwrap(), Some(target.clone()));
        assert!(!target.exists());
    }

    #[test]
    fn migration_removes_only_managed_blocks_and_preserves_unknown_skills() {
        let root =
            std::env::temp_dir().join(format!("luvus-skill-migration-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let codex = root.join(".codex");
        let opencode = root.join(".config/opencode");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(&opencode).unwrap();
        fs::write(
            codex.join("AGENTS.md"),
            format!("# Mine\n\n{POINTER_BEGIN}\nmanaged\n{POINTER_END}\n\nKeep me.\n"),
        )
        .unwrap();
        fs::write(
            opencode.join("AGENTS.md"),
            format!("User rule.\n\n{LEGACY_POINTER_BEGIN}\nlegacy\n{LEGACY_POINTER_END}\n"),
        )
        .unwrap();
        let modified = root.join(".claude/skills/luvus/SKILL.md");
        fs::create_dir_all(modified.parent().unwrap()).unwrap();
        fs::write(&modified, "user modified skill").unwrap();
        let marker = root.join(".luvus/skills").join(MIGRATION_MARKER);

        let changed = migrate_legacy_at(&root, None, None, &marker).unwrap();
        assert_eq!(changed.len(), 2);
        let codex_text = fs::read_to_string(codex.join("AGENTS.md")).unwrap();
        assert!(codex_text.contains("# Mine") && codex_text.contains("Keep me."));
        assert!(!codex_text.contains(POINTER_BEGIN));
        let opencode_text = fs::read_to_string(opencode.join("AGENTS.md")).unwrap();
        assert!(opencode_text.contains("User rule."));
        assert!(!opencode_text.contains(LEGACY_POINTER_BEGIN));
        assert_eq!(fs::read_to_string(modified).unwrap(), "user modified skill");
        assert!(marker.is_file());
        assert!(migrate_legacy_at(&root, None, None, &marker)
            .unwrap()
            .is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migration_strips_every_block_and_pairs_with_the_following_end() {
        let text = format!(
            "keep {POINTER_END}\n{POINTER_BEGIN}one{POINTER_END}\nmiddle\n\
             {POINTER_BEGIN}two{POINTER_END}\n{LEGACY_POINTER_BEGIN}old{LEGACY_POINTER_END}\nkeep"
        );
        let cleaned = strip_managed_blocks(&text);
        assert!(cleaned.starts_with(&format!("keep {POINTER_END}\n")));
        assert!(cleaned.contains("middle"));
        assert!(cleaned.ends_with("\nkeep"));
        assert!(!cleaned.contains(POINTER_BEGIN));
        assert!(!cleaned.contains(LEGACY_POINTER_BEGIN));
    }

    #[test]
    fn no_enabled_skills_means_update_needs_no_manifest() {
        let _env = crate::persist::test_env("skill-update-disabled");
        assert!(update(None, "https://invalid.example/never-fetched")
            .unwrap()
            .is_empty());
        let err = update(
            Some(SkillAgent::Codex),
            "https://invalid.example/never-fetched",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn skill_text_validation_is_strict_and_bounded() {
        assert!(validate_skill_text(valid_skill().as_bytes()).is_ok());
        assert!(validate_skill_text(b"<html>not a skill</html>").is_err());
    }
}
