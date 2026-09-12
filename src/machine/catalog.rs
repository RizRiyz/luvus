use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 256 * 1024;
pub(super) const MAX_PROFILES: usize = 64;
const MAX_ID_BYTES: usize = 48;
const MAX_LABEL_CHARS: usize = 64;
const MAX_DESTINATION_BYTES: usize = 255;
const MAX_REMOTE_BINARY_BYTES: usize = 1024;
pub(crate) const MAX_ENABLED_ENDPOINTS: usize = 16;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionPolicy {
    Manual,
    #[default]
    PersistentWhileOpen,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct MachineProfile {
    pub id: String,
    pub label: String,
    pub transport: String,
    pub destination: String,
    pub remote_binary: Option<String>,
    /// Legacy field identifying a managed binary rather than an explicit pin.
    /// This is provenance only, never permission to install or update. Every
    /// provisioning attempt requires fresh foreground approval.
    pub automatic_provisioning: bool,
    pub enabled: bool,
    pub preferred_session: Option<String>,
    /// Read-only migration input from the prototype. These entries never start
    /// connections and are omitted on the next explicit catalog mutation.
    #[serde(skip_serializing)]
    pub sessions: Vec<String>,
    pub connection_policy: ConnectionPolicy,
}

impl Default for MachineProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            transport: "ssh".to_string(),
            destination: String::new(),
            remote_binary: None,
            automatic_provisioning: false,
            enabled: true,
            preferred_session: None,
            sessions: Vec::new(),
            connection_policy: ConnectionPolicy::PersistentWhileOpen,
        }
    }
}

impl MachineProfile {
    /// Display labels do not participate in transport identity.
    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        self.id == other.id
            && self.transport == other.transport
            && self.destination == other.destination
            && self.remote_binary == other.remote_binary
            && self.automatic_provisioning == other.automatic_provisioning
            && self.enabled == other.enabled
            && self.preferred_session == other.preferred_session
            && self.connection_policy == other.connection_policy
    }

    pub(crate) fn new(id: String, destination: String) -> Self {
        Self {
            label: id.clone(),
            id,
            destination,
            ..Self::default()
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        validate_label(&self.label)?;
        if self.transport != "ssh" {
            return Err(anyhow!(
                "unsupported machine transport `{}`",
                self.transport
            ));
        }
        validate_destination(&self.destination)?;
        if let Some(binary) = self.remote_binary.as_deref() {
            validate_remote_binary(binary)?;
        }
        if let Some(session) = self.preferred_session.as_deref() {
            crate::session::validate_name(session).map_err(anyhow::Error::msg)?;
        }
        // Legacy session lists are bounded by the catalog byte limit and are
        // never used for routing. Do not let obsolete values block migration.
        Ok(())
    }

    pub(crate) fn session_names(&self) -> Vec<String> {
        let fallback = self
            .preferred_session
            .as_deref()
            .unwrap_or(crate::session::DEFAULT_SESSION_NAME);
        vec![fallback.to_string()]
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Catalog {
    pub format_version: u32,
    pub revision: u64,
    pub machines: Vec<MachineProfile>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            revision: 0,
            machines: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LoadedCatalog {
    pub catalog: Catalog,
    pub warnings: Vec<String>,
}

pub(super) struct CatalogLock {
    _file: File,
}

pub(crate) fn path() -> PathBuf {
    crate::persist::session_dir().join("machines.json")
}

pub(super) fn acquire_lock() -> Result<CatalogLock> {
    // A machine catalog belongs to the selected local named-session namespace,
    // just like its workspace snapshot and control sockets. Validate the
    // private session directory before creating the mutation lock.
    let root = crate::persist::ensure_server_session_dir()
        .context("cannot prepare the selected session machine catalog")?;
    let path = root.join("machines.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("cannot lock {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(CatalogLock { _file: file })
}

pub(crate) fn load() -> Result<LoadedCatalog> {
    load_from(&path())
}

fn load_from(path: &Path) -> Result<LoadedCatalog> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedCatalog {
                catalog: Catalog::default(),
                warnings: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if metadata.len() > MAX_CATALOG_BYTES {
        return Err(anyhow!(
            "machine catalog exceeds the {} byte limit",
            MAX_CATALOG_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CATALOG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(anyhow!("machine catalog exceeds the byte limit"));
    }
    let root: Value =
        serde_json::from_slice(&bytes).context("machine catalog is not valid JSON")?;
    let object = root
        .as_object()
        .ok_or_else(|| anyhow!("machine catalog root must be an object"))?;
    let format_version = object
        .get("format_version")
        .and_then(Value::as_u64)
        .unwrap_or(FORMAT_VERSION.into());
    if format_version > u64::from(FORMAT_VERSION) {
        return Err(anyhow!(
            "machine catalog format {format_version} is newer than this Luvus supports"
        ));
    }
    let revision = object.get("revision").and_then(Value::as_u64).unwrap_or(0);
    let empty = Vec::new();
    let rows = match object.get("machines") {
        Some(value) => value
            .as_array()
            .ok_or_else(|| anyhow!("machine catalog `machines` must be an array"))?,
        None => &empty,
    };
    if rows.len() > MAX_PROFILES {
        return Err(anyhow!(
            "machine catalog has more than {MAX_PROFILES} profiles"
        ));
    }

    let mut machines = Vec::with_capacity(rows.len());
    let mut warnings = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        match serde_json::from_value::<MachineProfile>(row.clone())
            .map_err(anyhow::Error::from)
            .and_then(|profile| {
                profile.validate()?;
                Ok(profile)
            }) {
            Ok(profile)
                if machines
                    .iter()
                    .any(|existing: &MachineProfile| existing.id == profile.id) =>
            {
                warnings.push(format!("machine entry {index} repeats id `{}`", profile.id));
            }
            Ok(profile) => {
                let enabled: usize = machines
                    .iter()
                    .filter(|machine| machine.enabled)
                    .map(|machine| machine.session_names().len())
                    .sum();
                if profile.enabled
                    && enabled.saturating_add(profile.session_names().len()) > MAX_ENABLED_ENDPOINTS
                {
                    warnings.push(format!(
                        "machine entry {index} exceeds the enabled endpoint limit"
                    ));
                } else {
                    machines.push(profile);
                }
            }
            Err(error) => warnings.push(format!("machine entry {index} was ignored: {error}")),
        }
    }
    Ok(LoadedCatalog {
        catalog: Catalog {
            format_version: FORMAT_VERSION,
            revision,
            machines,
        },
        warnings,
    })
}

pub(crate) fn mutate<T>(
    expected_revision: Option<u64>,
    operation: impl FnOnce(&mut Catalog) -> Result<T>,
) -> Result<(T, Catalog)> {
    let _lock = acquire_lock()?;
    let mut catalog = checked_for_mutation(load()?, expected_revision)?;
    let result = operation(&mut catalog)?;
    validate_catalog(&catalog)?;
    catalog.revision = catalog
        .revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("machine catalog revision is exhausted"))?;
    save(&catalog)?;
    Ok((result, catalog))
}

/// Reject invalid or stale catalog writes before a foreground operation starts
/// network or provisioning work. `mutate` repeats this check under the lock.
pub(crate) fn preflight_mutation(expected_revision: Option<u64>) -> Result<Catalog> {
    checked_for_mutation(load()?, expected_revision)
}

/// Check the proposed state before any remote side effects. The final write
/// must still fence the revision because network work runs without this lock.
pub(crate) fn preflight_profile(current: &Catalog, profile: &MachineProfile) -> Result<()> {
    let mut proposed = current.clone();
    if let Some(saved) = proposed
        .machines
        .iter_mut()
        .find(|saved| saved.id == profile.id)
    {
        *saved = profile.clone();
    } else {
        proposed.machines.push(profile.clone());
    }
    validate_catalog(&proposed)
}

/// Owner-local recovery information only; UHP projects errors separately.
pub(crate) fn prepared_commit_error(error: anyhow::Error, binary: &str) -> anyhow::Error {
    error.context(format!(
        "Remote preparation succeeded, but the machine catalog was not saved. The verified binary at `{binary}` may remain in use; it was not deleted or rolled back. Retry setup using that existing binary (--remote-binary for a new profile). If this operation installed the binary, `luvus machine list` includes its durable approved-installation receipt."
    ))
}

fn checked_for_mutation(loaded: LoadedCatalog, expected_revision: Option<u64>) -> Result<Catalog> {
    if !loaded.warnings.is_empty() {
        return Err(anyhow!(
            "machine catalog contains invalid entries; repair {} before changing it: {}",
            path().display(),
            loaded.warnings.join("; ")
        ));
    }
    let catalog = loaded.catalog;
    if let Some(expected) = expected_revision.filter(|expected| *expected != catalog.revision) {
        return Err(anyhow!(
            "machine catalog revision conflict: expected {}, current {}",
            expected,
            catalog.revision
        ));
    }
    Ok(catalog)
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    if catalog.machines.len() > MAX_PROFILES {
        return Err(anyhow!("at most {MAX_PROFILES} machines may be saved"));
    }
    let enabled_endpoints = catalog
        .machines
        .iter()
        .filter(|machine| machine.enabled)
        .map(|machine| machine.session_names().len())
        .sum::<usize>();
    if enabled_endpoints > MAX_ENABLED_ENDPOINTS {
        return Err(anyhow!(
            "at most {MAX_ENABLED_ENDPOINTS} remote sessions may be enabled"
        ));
    }
    for (index, machine) in catalog.machines.iter().enumerate() {
        machine.validate()?;
        if catalog.machines[..index]
            .iter()
            .any(|other| other.id == machine.id)
        {
            return Err(anyhow!("duplicate machine id `{}`", machine.id));
        }
    }
    Ok(())
}

fn save(catalog: &Catalog) -> Result<()> {
    let path = path();
    write_private_json(&path, &serde_json::to_vec_pretty(catalog)?)
}

pub(super) fn write_private_json(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("machine catalog has no parent directory"))?;
    fs::create_dir_all(parent)?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(anyhow!("machine catalog exceeds its size limit"));
    }
    let (temporary, mut file) = (0..16)
        .find_map(|_| {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary =
                path.with_extension(format!("json.{}.{sequence}.tmp", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temporary) {
                Ok(file) => Some(Ok((temporary, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| anyhow!("could not reserve a machine catalog temporary file"))?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        crate::platform::atomic_replace_file(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(anyhow!("machine id must contain 1 to {MAX_ID_BYTES} bytes"));
    }
    let mut bytes = id.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(anyhow!(
            "machine id must start with a lowercase letter or digit and contain only lowercase ASCII letters, digits, `.`, `_`, or `-`"
        ));
    }
    Ok(())
}

pub(crate) fn validate_label(label: &str) -> Result<()> {
    let count = label.chars().count();
    if count == 0 || count > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
        return Err(anyhow!(
            "machine label must contain 1 to {MAX_LABEL_CHARS} printable characters"
        ));
    }
    Ok(())
}

pub(super) fn validate_destination(destination: &str) -> Result<()> {
    if destination.is_empty()
        || destination.len() > MAX_DESTINATION_BYTES
        || destination.starts_with('-')
        || destination
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(anyhow!(
            "SSH destination must be 1 to {MAX_DESTINATION_BYTES} non-whitespace bytes and must not start with `-`"
        ));
    }
    Ok(())
}

pub(super) fn validate_remote_binary(binary: &str) -> Result<()> {
    let posix = binary.starts_with('/')
        && binary.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '-' | '.' | '+')
        });
    let bytes = binary.as_bytes();
    let windows = bytes.len() >= 4
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
        && binary[2..].chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, '\\' | '/' | '_' | '-' | '.' | '+' | ' ' | '\'')
        });
    if binary.len() > MAX_REMOTE_BINARY_BYTES || !(posix || windows) {
        return Err(anyhow!(
            "remote Luvus binary must be a shell-safe absolute POSIX or Windows drive path"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_write_failure_has_recovery_details_and_retains_cause() {
        let error = prepared_commit_error(
            anyhow!("machine catalog revision conflict: expected 1, current 2"),
            r"C:\Users\Alice Smith\luvus.exe",
        );
        assert!(error.to_string().contains("catalog was not saved"));
        assert!(error
            .to_string()
            .contains(r"C:\Users\Alice Smith\luvus.exe"));
        assert!(error.to_string().contains("luvus machine list"));
        assert!(error
            .to_string()
            .contains("If this operation installed the binary"));
        assert!(format!("{error:#}").contains("revision conflict"));
    }

    #[test]
    fn proposed_enabled_profile_is_rejected_before_preparation() {
        let current = Catalog {
            machines: (0..MAX_ENABLED_ENDPOINTS)
                .map(|n| MachineProfile::new(format!("box-{n}"), format!("box-{n}")))
                .collect(),
            ..Catalog::default()
        };
        let mut extra = MachineProfile::new("extra".into(), "extra".into());
        assert!(preflight_profile(&current, &extra).is_err());
        extra.enabled = false;
        assert!(preflight_profile(&current, &extra).is_ok());
        let mut full = current.clone();
        full.machines.push(extra.clone());
        extra.enabled = true;
        assert!(preflight_profile(&full, &extra).is_err());
    }

    #[test]
    fn label_changes_preserve_transport_identity() {
        let profile = MachineProfile::new("build".into(), "build-host".into());
        let mut renamed = profile.clone();
        renamed.label = "New display name".into();
        assert!(profile.same_connection(&renamed));
        renamed.destination = "different-host".into();
        assert!(!profile.same_connection(&renamed));
        renamed = profile.clone();
        renamed.sessions.push("review".into());
        assert!(profile.same_connection(&renamed));
        renamed.preferred_session = Some("review".into());
        assert!(!profile.same_connection(&renamed));
    }

    #[test]
    fn catalog_mutations_are_atomic_and_revision_checked() {
        let _env = crate::persist::test_env("machine-catalog-revision");
        let (_, first) = mutate(Some(0), |catalog| {
            catalog.machines.push(MachineProfile::new(
                "buildbox".into(),
                "dev@buildbox".into(),
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(load().unwrap().catalog, first);

        let error = mutate(Some(0), |_| Ok(())).unwrap_err().to_string();
        assert!(error.contains("revision conflict"));
        assert_eq!(load().unwrap().catalog, first);
    }

    #[test]
    fn named_sessions_have_independent_machine_catalogs() {
        let _env = crate::persist::test_env("machine-catalog-session-scope");
        let root = crate::persist::config_dir();
        assert_eq!(path(), root.join("machines.json"));

        std::env::set_var(crate::session::SESSION_ENV_VAR, "alpha");
        let (_, alpha) = mutate(Some(0), |catalog| {
            catalog.machines.push(MachineProfile::new(
                "alpha-box".into(),
                "alpha.example".into(),
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(path(), root.join("sessions/alpha/machines.json"));
        assert_eq!(alpha.machines.len(), 1);

        std::env::set_var(crate::session::SESSION_ENV_VAR, "beta");
        assert_eq!(path(), root.join("sessions/beta/machines.json"));
        assert!(load().unwrap().catalog.machines.is_empty());

        std::env::set_var(crate::session::SESSION_ENV_VAR, "alpha");
        assert_eq!(load().unwrap().catalog.machines[0].id, "alpha-box");
    }

    #[test]
    fn malformed_rows_are_isolated_and_block_lossy_mutation() {
        let _env = crate::persist::test_env("machine-catalog-malformed");
        crate::persist::ensure_config_dir();
        fs::write(
            path(),
            r#"{
  "format_version": 1,
  "revision": 7,
  "machines": [
    {"id":"good","label":"Good","transport":"ssh","destination":"good.example","enabled":false,"connection_policy":"manual"},
    {"id":"Bad ID","label":"Broken","transport":"ssh","destination":"bad.example"}
  ]
}"#,
        )
        .unwrap();
        let loaded = load().unwrap();
        assert_eq!(loaded.catalog.machines.len(), 1);
        assert!(!loaded.catalog.machines[0].automatic_provisioning);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(mutate(None, |_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(path())
                .unwrap()
                .matches("Bad ID")
                .count(),
            1
        );
    }

    #[test]
    fn older_profiles_gain_their_preferred_or_default_session_without_a_format_bump() {
        let preferred: MachineProfile = serde_json::from_str(
            r#"{
  "id":"build",
  "label":"Build",
  "transport":"ssh",
  "destination":"dev@build.example",
  "preferred_session":"review"
}"#,
        )
        .unwrap();
        assert_eq!(preferred.session_names(), vec!["review"]);

        let default: MachineProfile = serde_json::from_str(
            r#"{
  "id":"test",
  "label":"Test",
  "transport":"ssh",
  "destination":"dev@test.example"
}"#,
        )
        .unwrap();
        assert_eq!(
            default.session_names(),
            vec![crate::session::DEFAULT_SESSION_NAME]
        );
    }

    #[test]
    fn legacy_sessions_are_readable_but_never_persisted_or_connected() {
        let profile: MachineProfile = serde_json::from_value(serde_json::json!({
            "id": "build", "label": "Build", "destination": "build.example",
            "sessions": ["review", "t03", "t04", "t05", "t06", "t07", "t08", "t09", "t10"]
        }))
        .unwrap();
        assert_eq!(profile.session_names(), vec!["default"]);
        assert!(profile.validate().is_ok());
        let persisted = serde_json::to_value(&profile).unwrap();
        assert!(persisted.get("sessions").is_none());
    }

    #[test]
    fn saved_sessions_are_deduplicated_and_globally_bounded() {
        let mut machine = MachineProfile::new("build".into(), "dev@build.example".into());
        machine.preferred_session = Some("review".into());
        machine.sessions = vec!["review".into(), "release".into()];
        assert_eq!(machine.session_names(), vec!["review"]);
        assert!(machine.validate().is_ok());

        machine.sessions.push("release".into());
        machine
            .sessions
            .extend((0..100).map(|_| "obsolete invalid/name".into()));
        assert!(machine.validate().is_ok());

        let catalog = Catalog {
            machines: (0..=MAX_ENABLED_ENDPOINTS)
                .map(|index| MachineProfile::new(format!("box-{index}"), format!("box-{index}")))
                .collect(),
            ..Catalog::default()
        };
        let error = validate_catalog(&catalog).unwrap_err().to_string();
        assert!(error.contains("remote sessions may be enabled"));
    }

    #[test]
    fn destinations_and_remote_paths_cannot_be_ssh_options_or_shell_fragments() {
        assert!(validate_destination("dev@buildbox").is_ok());
        assert!(validate_destination("-oProxyCommand=bad").is_err());
        assert!(validate_destination("host;touch-pwned").is_ok());
        assert!(validate_destination("host name").is_err());
        assert!(validate_remote_binary("/home/dev/.local/bin/luvus").is_ok());
        assert!(validate_remote_binary(r"C:\Users\dev\AppData\Local\luvus\luvus.exe").is_ok());
        assert!(validate_remote_binary("C:/Users/dev/.local/bin/luvus.exe").is_ok());
        assert!(validate_remote_binary(r"C:\Program Files\luvus.exe").is_ok());
        assert!(validate_remote_binary(r"C:\temp\luvus.exe:stream").is_err());
        assert!(validate_remote_binary(r"C:\temp\luvus.exe & whoami").is_err());
        assert!(validate_remote_binary("luvus").is_err());
        assert!(validate_remote_binary("/tmp/luvus;bad").is_err());
    }
}
