//! One bundled Luvus skill with host-specific installation adapters.
//!
//! The skill is part of the Luvus release, so its instructions always match the
//! binary that installs them. `luvus skill enable` copies the same logical skill
//! into each detected supported skill host. No command here downloads prompt
//! instructions or changes an agent configuration during Luvus startup.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const BUNDLED_SKILL: &str = include_str!("../skills/luvus/SKILL.md");
const BUNDLED_ADVANCED_CONTROL: &str =
    include_str!("../skills/luvus/references/advanced-control.md");
const BUNDLED_UHP_CONTROL: &str = include_str!("../skills/luvus/references/uhp-control.md");
const BUNDLED_OPENAI_METADATA: &str = include_str!("../skills/luvus/agents/openai.yaml");

const STATE_SCHEMA: u32 = 1;
const BUNDLED_SOURCE: &str = "bundled";
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

/// A native location capable of loading the one bundled Luvus skill.
///
/// This is intentionally an installation detail, not a user-selectable skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillHost {
    Claude,
    /// The open `~/.agents/skills` location shared by Codex, Copilot, Gemini,
    /// Pi, Cursor, Amp, Droid, and fx.
    Shared,
    Opencode,
    Kimi,
    Grok,
    Qwen,
    Kiro,
    Omp,
}

impl SkillHost {
    const ALL: [Self; 8] = [
        Self::Shared,
        Self::Claude,
        Self::Opencode,
        Self::Kimi,
        Self::Grok,
        Self::Qwen,
        Self::Kiro,
        Self::Omp,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Shared => "shared",
            Self::Opencode => "opencode",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Qwen => "qwen",
            Self::Kiro => "kiro",
            Self::Omp => "omp",
        }
    }

    /// Preserve the schema-1 `codex` ownership key for the shared destination.
    /// Older Luvus releases already managed the same `~/.agents/skills` path,
    /// so changing the key would orphan their ownership record.
    fn state_key(self) -> &'static str {
        if self == Self::Shared {
            "codex"
        } else {
            self.as_str()
        }
    }
}

impl fmt::Display for SkillHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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

/// Keep the schema-1 `agents` field so state written by Luvus 0.11 and 0.12 is
/// read without migration. Keys now identify installation hosts, not separate
/// logical skills.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationState {
    Current,
    Outdated,
    Missing,
    Modified,
    ExternalCurrent,
    External,
    Available,
    NotDetected,
}

impl DestinationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Outdated => "outdated",
            Self::Missing => "missing",
            Self::Modified => "modified",
            Self::ExternalCurrent => "external-current",
            Self::External => "external",
            Self::Available => "not-installed",
            Self::NotDetected => "not-detected",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DestinationStatus {
    pub host: SkillHost,
    pub target: PathBuf,
    pub state: DestinationState,
    pub managed_release: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    Installed,
    Refreshed,
    Repaired,
    Current,
    External,
    PreservedModified,
    Disabled,
    AlreadyDisabled,
}

impl ChangeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Refreshed => "refreshed",
            Self::Repaired => "repaired",
            Self::Current => "current",
            Self::External => "external-preserved",
            Self::PreservedModified => "modified-preserved",
            Self::Disabled => "disabled",
            Self::AlreadyDisabled => "already-disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillChange {
    pub host: SkillHost,
    pub target: PathBuf,
    pub action: ChangeAction,
}

#[derive(Clone, Copy)]
struct BundledFile {
    path: &'static str,
    content: &'static str,
}

fn bundled_files(host: SkillHost) -> Vec<BundledFile> {
    let mut files = vec![
        BundledFile {
            path: "SKILL.md",
            content: BUNDLED_SKILL,
        },
        BundledFile {
            path: "references/advanced-control.md",
            content: BUNDLED_ADVANCED_CONTROL,
        },
        BundledFile {
            path: "references/uhp-control.md",
            content: BUNDLED_UHP_CONTROL,
        },
    ];
    if host == SkillHost::Shared {
        files.push(BundledFile {
            path: "agents/openai.yaml",
            content: BUNDLED_OPENAI_METADATA,
        });
    }
    files
}

fn bundled_managed_files(host: SkillHost) -> Vec<ManagedFile> {
    bundled_files(host)
        .into_iter()
        .map(|file| ManagedFile {
            path: file.path.to_string(),
            sha256: sha256_hex(file.content.as_bytes()),
        })
        .collect()
}

pub fn bundled_release() -> &'static str {
    env!("CARGO_PKG_VERSION")
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

fn target_dir_at(host: SkillHost, home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    match host {
        SkillHost::Claude => home.join(".claude").join("skills").join("luvus"),
        SkillHost::Shared => home.join(".agents").join("skills").join("luvus"),
        SkillHost::Opencode => xdg_config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode")
            .join("skills")
            .join("luvus"),
        SkillHost::Kimi => home.join(".kimi-code").join("skills").join("luvus"),
        SkillHost::Grok => home.join(".grok").join("skills").join("luvus"),
        SkillHost::Qwen => home.join(".qwen").join("skills").join("luvus"),
        SkillHost::Kiro => home.join(".kiro").join("skills").join("luvus"),
        SkillHost::Omp => home.join(".omp").join("agent").join("skills").join("luvus"),
    }
}

fn target_dir(host: SkillHost) -> Result<PathBuf> {
    let home = crate::platform::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    if host == SkillHost::Claude {
        if let Some(config) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Ok(PathBuf::from(config).join("skills").join("luvus"));
        }
    }
    if host == SkillHost::Kimi {
        if let Some(config) = std::env::var_os("KIMI_CODE_HOME") {
            return Ok(PathBuf::from(config).join("skills").join("luvus"));
        }
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(target_dir_at(host, &home, xdg.as_deref()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_hex(&bytes))
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

fn record_matches_bundle(record: &InstalledSkill, host: SkillHost) -> Result<bool> {
    Ok(record.release == bundled_release()
        && record.source == BUNDLED_SOURCE
        && record.files == bundled_managed_files(host)
        && integrity(record)? == Integrity::Current)
}

fn external_matches_bundle(path: &Path, host: SkillHost) -> Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    for file in bundled_files(host) {
        let candidate = path.join(file.path);
        if !candidate.is_file() || hash_file(&candidate)? != sha256_hex(file.content.as_bytes()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn command_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let mut candidates = vec![name.to_string()];
    if cfg!(windows) {
        let extensions = std::env::var_os("PATHEXT")
            .and_then(|value| value.into_string().ok())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
        candidates.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{name}{}", extension.to_ascii_lowercase())),
        );
        candidates.extend([format!("{name}.exe"), format!("{name}.cmd")]);
    }
    std::env::split_paths(&path).any(|dir| {
        candidates
            .iter()
            .any(|candidate| dir.join(candidate).is_file())
    })
}

fn any_command_on_path(names: &[&str]) -> bool {
    names.iter().any(|name| command_on_path(name))
}

fn any_dir(paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path.is_dir())
}

fn host_commands(host: SkillHost) -> &'static [&'static str] {
    match host {
        SkillHost::Claude => &["claude"],
        SkillHost::Shared => &[
            "codex",
            "copilot",
            "gemini",
            "pi-coding-agent",
            "cursor-agent",
            "amp",
            "droid",
            "fx",
        ],
        SkillHost::Opencode => &["opencode"],
        SkillHost::Kimi => &["kimi"],
        SkillHost::Grok => &["grok"],
        SkillHost::Qwen => &["qwen"],
        SkillHost::Kiro => &["kiro", "kiro-cli"],
        SkillHost::Omp => &["omp"],
    }
}

fn host_config_dirs(
    host: SkillHost,
    home: &Path,
    xdg_config: Option<&Path>,
    claude_config: Option<&Path>,
    codex_home: Option<&Path>,
    kimi_home: Option<&Path>,
) -> Vec<PathBuf> {
    let xdg = xdg_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    match host {
        SkillHost::Claude => vec![claude_config
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".claude"))],
        SkillHost::Shared => vec![
            codex_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".codex")),
            home.join(".agents"),
            home.join(".copilot"),
            home.join(".gemini"),
            home.join(".pi").join("agent"),
            home.join(".cursor"),
            xdg.join("amp"),
            home.join(".factory"),
            home.join(".fx"),
        ],
        SkillHost::Opencode => vec![xdg.join("opencode")],
        SkillHost::Kimi => vec![kimi_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".kimi-code"))],
        SkillHost::Grok => vec![home.join(".grok")],
        SkillHost::Qwen => vec![home.join(".qwen")],
        SkillHost::Kiro => vec![home.join(".kiro")],
        SkillHost::Omp => vec![home.join(".omp").join("agent")],
    }
}

fn host_detected(host: SkillHost, state: &SkillState, target: &Path) -> Result<bool> {
    if state.agents.contains_key(host.state_key()) || target.exists() {
        return Ok(true);
    }
    let home = crate::platform::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    let kimi = std::env::var_os("KIMI_CODE_HOME").map(PathBuf::from);
    Ok(any_dir(&host_config_dirs(
        host,
        &home,
        xdg.as_deref(),
        claude.as_deref(),
        codex.as_deref(),
        kimi.as_deref(),
    )) || any_command_on_path(host_commands(host)))
}

fn stage_bundle(target: &Path, host: SkillHost) -> Result<(PathBuf, Vec<ManagedFile>)> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("skill target has no parent"))?;
    ensure_private_dir(parent)?;
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stage = parent.join(format!(
        ".luvus-skill-stage-{host}-{}-{sequence}",
        std::process::id()
    ));
    if stage.exists() {
        fs::remove_dir_all(&stage)
            .with_context(|| format!("removing stale stage {}", stage.display()))?;
    }
    ensure_private_dir(&stage)?;

    let result = (|| {
        let files = bundled_files(host);
        let mut managed = Vec::with_capacity(files.len());
        for file in files {
            let path = stage.join(file.path);
            if let Some(dir) = path.parent() {
                ensure_private_dir(dir)?;
            }
            let mut output = File::create(&path)
                .with_context(|| format!("creating staged skill file {}", path.display()))?;
            output
                .write_all(file.content.as_bytes())
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
                path: file.path.to_string(),
                sha256: sha256_hex(file.content.as_bytes()),
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

fn install_one_at(state: &mut SkillState, host: SkillHost, target: PathBuf) -> Result<SkillChange> {
    let key = host.state_key();
    let previous = state.agents.get(key).cloned();
    let previous_integrity = previous.as_ref().map(integrity).transpose()?;

    if let Some(record) = previous.as_ref() {
        if previous_integrity == Some(Integrity::Modified) {
            return Ok(SkillChange {
                host,
                target: record.target.clone(),
                action: ChangeAction::PreservedModified,
            });
        }
        if record.target == target && record_matches_bundle(record, host)? {
            return Ok(SkillChange {
                host,
                target,
                action: ChangeAction::Current,
            });
        }
        if record.target != target && target.exists() {
            return Ok(SkillChange {
                host,
                target,
                action: ChangeAction::External,
            });
        }
    } else if target.exists() {
        return Ok(SkillChange {
            host,
            target,
            action: ChangeAction::External,
        });
    }

    let (stage, files) = stage_bundle(&target, host)?;
    // Staging can take long enough for another process to replace a missing
    // destination. Recheck ownership before moving or deleting anything.
    let managed_source = match previous.as_ref() {
        Some(record) => match integrity(record)? {
            Integrity::Modified => {
                let _ = fs::remove_dir_all(&stage);
                return Ok(SkillChange {
                    host,
                    target: record.target.clone(),
                    action: ChangeAction::PreservedModified,
                });
            }
            Integrity::Current => Some(record.target.clone()),
            Integrity::Missing => None,
        },
        None => None,
    };
    if managed_source.as_ref() != Some(&target) && target.exists() {
        let _ = fs::remove_dir_all(&stage);
        return Ok(SkillChange {
            host,
            target,
            action: ChangeAction::External,
        });
    }

    let backup = managed_source.as_ref().map(|source| {
        let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        source
            .parent()
            .expect("managed target parent checked")
            .join(format!(
                ".luvus-skill-backup-{host}-{}-{sequence}",
                std::process::id()
            ))
    });
    if let (Some(source), Some(backup)) = (managed_source.as_ref(), backup.as_ref()) {
        if let Err(err) = fs::rename(source, backup) {
            let _ = fs::remove_dir_all(&stage);
            return Err(err).with_context(|| format!("backing up {}", source.display()));
        }
    }
    if target.exists() {
        let _ = fs::remove_dir_all(&stage);
        if let (Some(source), Some(backup)) = (managed_source.as_ref(), backup.as_ref()) {
            if !source.exists() {
                let _ = fs::rename(backup, source);
            }
        }
        bail!(
            "{} appeared while installing the {host} skill; existing content was preserved",
            target.display()
        );
    }
    if let Err(err) = fs::rename(&stage, &target) {
        if let (Some(source), Some(backup)) = (managed_source.as_ref(), backup.as_ref()) {
            if !source.exists() {
                let _ = fs::rename(backup, source);
            }
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(err).with_context(|| format!("installing {host} skill adapter"));
    }

    let installed = InstalledSkill {
        release: bundled_release().to_string(),
        source: BUNDLED_SOURCE.to_string(),
        target: target.clone(),
        files,
    };
    state.agents.insert(key.to_string(), installed.clone());
    if let Err(err) = save_state(state) {
        if matches!(integrity(&installed), Ok(Integrity::Current)) {
            let _ = fs::remove_dir_all(&target);
        }
        if let (Some(source), Some(backup)) = (managed_source.as_ref(), backup.as_ref()) {
            if !source.exists() {
                let _ = fs::rename(backup, source);
            }
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
    if let (Some(record), Some(backup)) = (previous.as_ref(), backup.as_ref()) {
        if backup.exists() {
            let mut backed_up_record = record.clone();
            backed_up_record.target = backup.clone();
            if integrity(&backed_up_record)? == Integrity::Current {
                fs::remove_dir_all(backup)
                    .with_context(|| format!("removing managed backup {}", backup.display()))?;
            } else {
                bail!(
                    "managed backup changed during installation and was preserved at {}",
                    backup.display()
                );
            }
        }
    }

    let action = match previous_integrity {
        None => ChangeAction::Installed,
        Some(Integrity::Missing) => ChangeAction::Repaired,
        Some(Integrity::Current) => ChangeAction::Refreshed,
        Some(Integrity::Modified) => unreachable!("modified installations are preserved above"),
    };
    Ok(SkillChange {
        host,
        target,
        action,
    })
}

/// Install or refresh the one bundled skill in every detected supported host.
pub fn enable() -> Result<Vec<SkillChange>> {
    let lock = lock_state()?;
    let mut state = load_state()?;
    let mut selected = Vec::new();
    for host in SkillHost::ALL {
        let target = target_dir(host)?;
        if host_detected(host, &state, &target)? {
            selected.push((host, target));
        }
    }
    if selected.is_empty() {
        bail!(
            "no native Agent Skills host detected; use `luvus skill show` for this session or install a supported skill-capable agent, then run `luvus skill enable`"
        );
    }

    let mut changes = Vec::with_capacity(selected.len());
    for (host, target) in selected {
        changes.push(install_one_at(&mut state, host, target)?);
    }
    FileExt::unlock(&lock).ok();
    Ok(changes)
}

fn disable_one_at(
    state: &mut SkillState,
    host: SkillHost,
    external_target: PathBuf,
) -> Result<SkillChange> {
    let Some(record) = state.agents.get(host.state_key()).cloned() else {
        return Ok(SkillChange {
            host,
            action: if external_target.exists() {
                ChangeAction::External
            } else {
                ChangeAction::AlreadyDisabled
            },
            target: external_target,
        });
    };
    if record.target.exists() && integrity(&record)? != Integrity::Current {
        return Ok(SkillChange {
            host,
            target: record.target,
            action: ChangeAction::PreservedModified,
        });
    }

    let parent = record
        .target
        .parent()
        .ok_or_else(|| anyhow!("managed skill target has no parent"))?;
    let sequence = INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let backup = parent.join(format!(
        ".luvus-skill-disable-{host}-{}-{sequence}",
        std::process::id()
    ));
    if record.target.exists() {
        fs::rename(&record.target, &backup)
            .with_context(|| format!("staging removal of {}", record.target.display()))?;
    }
    state.agents.remove(host.state_key());
    if let Err(err) = save_state(state) {
        if backup.exists() {
            let _ = fs::rename(&backup, &record.target);
        }
        state.agents.insert(host.state_key().to_string(), record);
        return Err(err).context("saving disabled skill state; removal rolled back");
    }
    if backup.exists() {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(SkillChange {
        host,
        target: record.target,
        action: ChangeAction::Disabled,
    })
}

/// Remove every unchanged Luvus-managed installation. External and modified
/// copies are always preserved.
pub fn disable() -> Result<Vec<SkillChange>> {
    let lock = lock_state()?;
    let mut state = load_state()?;
    let mut changes = Vec::with_capacity(SkillHost::ALL.len());
    for host in SkillHost::ALL {
        changes.push(disable_one_at(&mut state, host, target_dir(host)?)?);
    }
    FileExt::unlock(&lock).ok();
    Ok(changes)
}

pub fn status() -> Result<Vec<DestinationStatus>> {
    let state = load_state()?;
    SkillHost::ALL
        .into_iter()
        .map(|host| {
            let target = target_dir(host)?;
            let detected = host_detected(host, &state, &target)?;
            let record = state.agents.get(host.state_key());
            let (state_value, managed_release) = match record {
                Some(record) => {
                    let integrity = integrity(record)?;
                    let value = match integrity {
                        Integrity::Missing => DestinationState::Missing,
                        Integrity::Modified => DestinationState::Modified,
                        Integrity::Current if record_matches_bundle(record, host)? => {
                            DestinationState::Current
                        }
                        Integrity::Current => DestinationState::Outdated,
                    };
                    (value, Some(record.release.clone()))
                }
                None if target.exists() && external_matches_bundle(&target, host)? => {
                    (DestinationState::ExternalCurrent, None)
                }
                None if target.exists() => (DestinationState::External, None),
                None if detected => (DestinationState::Available, None),
                None => (DestinationState::NotDetected, None),
            };
            Ok(DestinationStatus {
                host,
                target: record.map_or(target, |record| record.target.clone()),
                state: state_value,
                managed_release,
            })
        })
        .collect()
}

/// Return the canonical, version-matched skill regardless of installation
/// state. `luvus skill show` writes this value to stdout.
pub fn show() -> &'static str {
    BUNDLED_SKILL
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
/// production agent configuration. Tests exercise `migrate_legacy_at` with
/// explicit isolated paths.
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

    #[test]
    fn canonical_and_plugin_skill_artifacts_stay_identical() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let plugin = root.join("plugins/luvus/skills/luvus");
        if !plugin.is_dir() {
            return;
        }
        assert_eq!(
            fs::read_to_string(plugin.join("SKILL.md")).unwrap(),
            BUNDLED_SKILL
        );
        assert_eq!(
            fs::read_to_string(plugin.join("references/advanced-control.md")).unwrap(),
            BUNDLED_ADVANCED_CONTROL
        );
        assert_eq!(
            fs::read_to_string(plugin.join("references/uhp-control.md")).unwrap(),
            BUNDLED_UHP_CONTROL
        );
        assert_eq!(
            fs::read_to_string(plugin.join("agents/openai.yaml")).unwrap(),
            BUNDLED_OPENAI_METADATA
        );
    }

    #[test]
    fn native_targets_are_internal_adapters_not_agents_md() {
        let home = Path::new("/home/tester");
        assert_eq!(
            target_dir_at(SkillHost::Shared, home, None),
            PathBuf::from("/home/tester/.agents/skills/luvus")
        );
        assert!(!target_dir_at(SkillHost::Shared, home, None).ends_with("AGENTS.md"));
        assert!(!target_dir_at(SkillHost::Opencode, home, None).ends_with("AGENTS.md"));
        assert_eq!(
            target_dir_at(SkillHost::Opencode, home, Some(Path::new("/xdg/config"))),
            PathBuf::from("/xdg/config/opencode/skills/luvus")
        );
        assert_eq!(
            target_dir_at(SkillHost::Kimi, home, None),
            PathBuf::from("/home/tester/.kimi-code/skills/luvus")
        );
        assert_eq!(
            target_dir_at(SkillHost::Grok, home, None),
            PathBuf::from("/home/tester/.grok/skills/luvus")
        );
        assert_eq!(
            target_dir_at(SkillHost::Qwen, home, None),
            PathBuf::from("/home/tester/.qwen/skills/luvus")
        );
        assert_eq!(
            target_dir_at(SkillHost::Kiro, home, None),
            PathBuf::from("/home/tester/.kiro/skills/luvus")
        );
        assert_eq!(SkillHost::Shared.state_key(), "codex");
    }

    #[test]
    fn every_native_skill_host_has_detection_markers() {
        for host in SkillHost::ALL {
            assert!(!host_commands(host).is_empty());
            assert!(
                !host_config_dirs(host, Path::new("/home/tester"), None, None, None, None)
                    .is_empty()
            );
        }
        let shared = host_commands(SkillHost::Shared);
        for agent in [
            "codex",
            "copilot",
            "gemini",
            "pi-coding-agent",
            "cursor-agent",
            "amp",
            "droid",
            "fx",
        ] {
            assert!(
                shared.contains(&agent),
                "missing shared host detection for {agent}"
            );
        }
    }

    #[test]
    fn bundled_install_refresh_and_modified_preservation_are_safe() {
        let _env = crate::persist::test_env("skill-bundled-install");
        let target = crate::persist::skills_dir().join("agent-home/skills/luvus");
        let mut state = SkillState::default();

        let installed = install_one_at(&mut state, SkillHost::Shared, target.clone()).unwrap();
        assert_eq!(installed.action, ChangeAction::Installed);
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, target.clone())
                .unwrap()
                .action,
            ChangeAction::Current
        );

        state.agents.get_mut("codex").unwrap().release = "0.1.0".into();
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, target.clone())
                .unwrap()
                .action,
            ChangeAction::Refreshed
        );

        fs::write(target.join("SKILL.md"), "user changed this skill").unwrap();
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, target.clone())
                .unwrap()
                .action,
            ChangeAction::PreservedModified
        );
        assert_eq!(
            disable_one_at(&mut state, SkillHost::Shared, target.clone())
                .unwrap()
                .action,
            ChangeAction::PreservedModified
        );
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "user changed this skill"
        );
    }

    #[test]
    fn changed_host_path_moves_only_an_unmodified_managed_skill() {
        let _env = crate::persist::test_env("skill-host-path-change");
        let root = crate::persist::skills_dir().join("host-path-change");
        let old_target = root.join("old/luvus");
        let new_target = root.join("new/luvus");
        let blocked_target = root.join("blocked/luvus");
        let mut state = SkillState::default();

        install_one_at(&mut state, SkillHost::Shared, old_target.clone()).unwrap();
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, new_target.clone())
                .unwrap()
                .action,
            ChangeAction::Refreshed
        );
        assert!(!old_target.exists());
        assert_eq!(state.agents["codex"].target, new_target);

        fs::create_dir_all(&blocked_target).unwrap();
        fs::write(blocked_target.join("SKILL.md"), "external replacement").unwrap();
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, blocked_target.clone())
                .unwrap()
                .action,
            ChangeAction::External
        );
        assert!(new_target.exists());
        assert_eq!(
            fs::read_to_string(blocked_target.join("SKILL.md")).unwrap(),
            "external replacement"
        );

        let later_target = root.join("later/luvus");
        fs::write(new_target.join("SKILL.md"), "managed copy was edited").unwrap();
        assert_eq!(
            install_one_at(&mut state, SkillHost::Shared, later_target.clone())
                .unwrap()
                .action,
            ChangeAction::PreservedModified
        );
        assert!(!later_target.exists());
        assert_eq!(
            fs::read_to_string(new_target.join("SKILL.md")).unwrap(),
            "managed copy was edited"
        );
    }

    #[test]
    fn missing_managed_target_never_overwrites_replacement_content() {
        let _env = crate::persist::test_env("skill-missing-replacement");
        let target = crate::persist::skills_dir().join("missing-replacement/luvus");
        let mut state = SkillState::default();

        install_one_at(&mut state, SkillHost::Claude, target.clone()).unwrap();
        fs::remove_dir_all(&target).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "replacement content").unwrap();

        assert_eq!(
            install_one_at(&mut state, SkillHost::Claude, target.clone())
                .unwrap()
                .action,
            ChangeAction::PreservedModified
        );
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "replacement content"
        );
    }

    #[test]
    fn external_skill_is_never_overwritten_or_removed() {
        let _env = crate::persist::test_env("skill-external-install");
        let target = crate::persist::skills_dir().join("external/luvus");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "external skill").unwrap();
        let mut state = SkillState::default();

        assert_eq!(
            install_one_at(&mut state, SkillHost::Claude, target.clone())
                .unwrap()
                .action,
            ChangeAction::External
        );
        assert_eq!(
            disable_one_at(&mut state, SkillHost::Claude, target.clone())
                .unwrap()
                .action,
            ChangeAction::External
        );
        assert_eq!(
            fs::read_to_string(target.join("SKILL.md")).unwrap(),
            "external skill"
        );
    }

    #[test]
    fn show_is_the_canonical_release_matched_skill() {
        assert_eq!(show(), BUNDLED_SKILL);
        assert!(show().contains("name: luvus"));
        assert!(show().contains("agent send"));
        assert!(show().contains("luvus uhp capabilities"));
        assert!(show().contains("luvus mission open"));
        assert!(show().contains("mission.open"));
        assert!(show().contains("ui.bar.*"));
        assert!(show().contains("Agent detection is built into Luvus"));
    }

    #[test]
    fn migration_removes_only_managed_blocks_and_preserves_unknown_skills() {
        let _env = crate::persist::test_env("skill-migration");
        let root = crate::persist::skills_dir().join("migration-home");
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
    fn omp_is_a_recognized_skill_host_by_dir_or_binary() {
        // Detection must fire on either signal: the ~/.omp/agent config dir
        // or the omp executable on PATH. OMP reads the shared ~/.agents/skills
        // location, so `skill enable` covers it once detected.
        let root = crate::persist::skills_dir().join("omp-host-home");
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join(".omp").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();

        let dirs = host_config_dirs(SkillHost::Omp, &root, None, None, None, None);
        assert_eq!(dirs, vec![agent_dir.clone()]);
        assert!(any_dir(&dirs), "config dir presence detects the omp host");
        assert_eq!(host_commands(SkillHost::Omp), &["omp"]);
        assert_eq!(SkillHost::Omp.as_str(), "omp");
        assert!(SkillHost::ALL.contains(&SkillHost::Omp));

        let _ = fs::remove_dir_all(&root);
    }
}
