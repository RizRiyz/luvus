use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::format::{validate_id, ThemeFile, MAX_FILE_BYTES};
use super::registry::{is_reserved_id, validate_standalone, ThemeRegistry};

pub const COMMUNITY_PREFIX: &str = "community/";
const COMMUNITY_RAW_ROOT: &str =
    "https://raw.githubusercontent.com/RizRiyz/luvus/main/community/themes";
const GITHUB_API_ROOT: &str = "https://api.github.com/repos";
const MAX_GITHUB_INDEX_BYTES: usize = 64 * 1024;
static NEXT_TMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct InstalledTheme {
    pub id: String,
    pub display_name: String,
    pub path: PathBuf,
    pub source: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubContent {
    name: String,
    path: String,
    #[serde(rename = "type")]
    kind: String,
    size: u64,
    download_url: Option<String>,
    html_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum GitHubSource {
    Repository {
        owner: String,
        repository: String,
        canonical: String,
    },
    File {
        download: String,
        canonical: String,
    },
}

#[derive(Debug, Serialize)]
struct Provenance<'a> {
    schema: u32,
    source: &'a str,
    sha256: String,
    installed_unix: u64,
}

pub fn install(source: &str, yes: bool) -> Result<InstalledTheme> {
    let (bytes, canonical_source, remote) = acquire(source)?;
    let file = ThemeFile::parse(&bytes)?;
    if is_reserved_id(&file.id) {
        bail!("theme ID `{}` is reserved by Luvus", file.id);
    }
    let registry = ThemeRegistry::load();
    let (_, warnings) = validate_standalone(&file, &registry)?;

    if remote && !yes {
        confirm(&file, &canonical_source)?;
    }

    let dir = super::ensure_themes_dir()?;
    let destination = dir.join(format!("{}.toml", file.id));
    reject_duplicate_destination(&registry, &file.id, &destination)?;
    let provenance = provenance_path(&dir, &file.id);
    let previous_theme = fs::read(&destination).ok();
    let previous_provenance = fs::read(&provenance).ok();

    let transaction = (|| -> Result<()> {
        atomic_write(&destination, &bytes)?;
        write_provenance(&dir, &file.id, &canonical_source, &bytes)?;

        // Validate the exact on-disk registry before reporting success. This
        // catches conflicts with manually copied files and resolution changes.
        let loaded = ThemeRegistry::load_from(&dir);
        if loaded.get(&file.id).is_none() {
            let message = loaded
                .problems()
                .iter()
                .find(|problem| problem.path == destination.display().to_string())
                .map(|problem| problem.message.clone())
                .unwrap_or_else(|| "theme did not load from the installed registry".to_string());
            bail!("installed theme failed registry validation: {message}");
        }
        Ok(())
    })();
    if let Err(error) = transaction {
        restore_file(&destination, previous_theme.as_deref());
        restore_file(&provenance, previous_provenance.as_deref());
        return Err(error);
    }

    Ok(InstalledTheme {
        id: file.id,
        display_name: file.display_name,
        path: destination,
        source: canonical_source,
        warnings,
    })
}

pub fn uninstall(id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    if is_reserved_id(id) {
        bail!("`{id}` is bundled with Luvus and cannot be uninstalled");
    }
    let config = crate::config::load();
    if crate::ui::theme::canonical(&config.theme) == id {
        bail!("cannot uninstall active theme `{id}`; run `luvus theme use <other-id>` first");
    }
    let dir = super::themes_dir();
    let registry = ThemeRegistry::load_from(&dir);
    let entry = registry
        .get(id)
        .with_context(|| format!("theme `{id}` is not installed"))?;
    let path = match &entry.source {
        super::registry::ThemeSource::Local { path, .. } => PathBuf::from(path),
        _ => bail!("`{id}` is not a local theme"),
    };

    let dependents = local_dependents(&dir, id);
    if !dependents.is_empty() {
        bail!(
            "cannot uninstall `{id}`; required by {}",
            dependents.join(", ")
        );
    }
    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    let _ = fs::remove_file(provenance_path(&dir, id));
    Ok(path)
}

pub fn init(path: &Path, id: &str, extends: Option<&str>) -> Result<()> {
    validate_id(id)?;
    if is_reserved_id(id) {
        bail!("theme ID `{id}` is reserved by Luvus");
    }
    if let Some(parent) = extends {
        validate_id(parent).context("invalid parent theme ID")?;
        if ThemeRegistry::load().get(parent).is_none() {
            bail!("parent theme `{parent}` is not installed");
        }
    }
    let body = starter_toml(id, extends);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub fn validate_path(path: &Path, strict: bool) -> Result<(ThemeFile, Vec<String>)> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        bail!("theme file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file = ThemeFile::parse(&bytes)?;
    let (_, warnings) = validate_standalone(&file, &ThemeRegistry::load())?;
    if strict && !warnings.is_empty() {
        bail!("strict validation failed: {}", warnings.join("; "));
    }
    Ok((file, warnings))
}

pub fn community_url(id: &str) -> Result<String> {
    validate_id(id)?;
    Ok(format!("{COMMUNITY_RAW_ROOT}/{id}.toml"))
}

fn acquire(source: &str) -> Result<(Vec<u8>, String, bool)> {
    if let Some(id) = source.strip_prefix(COMMUNITY_PREFIX) {
        if id.is_empty() || id.contains('/') {
            bail!("community source must use `community/<theme-id>`");
        }
        let url = community_url(id)?;
        return Ok((fetch_https(&url)?, url, true));
    }
    if source.starts_with("https://") {
        if let Some(github) = parse_github_source(source)? {
            return match github {
                GitHubSource::Repository {
                    owner,
                    repository,
                    canonical,
                } => acquire_github_repository(&owner, &repository, &canonical),
                GitHubSource::File {
                    download,
                    canonical,
                } => Ok((fetch_https(&download)?, canonical, true)),
            };
        }
        return Ok((fetch_https(source)?, source.to_string(), true));
    }
    if source.contains("://") {
        bail!("remote theme sources must use HTTPS");
    }
    let path = PathBuf::from(source);
    let metadata = fs::metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("theme source is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        bail!("theme file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let source = fs::canonicalize(&path)
        .unwrap_or(path)
        .display()
        .to_string();
    Ok((bytes, source, false))
}

fn parse_github_source(source: &str) -> Result<Option<GitHubSource>> {
    let rest = if let Some(rest) = source.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = source.strip_prefix("https://www.github.com/") {
        rest
    } else {
        return Ok(None);
    };
    if rest.contains(['?', '#']) {
        bail!("GitHub theme URLs cannot contain a query or fragment");
    }
    let rest = rest.trim_end_matches('/');
    let parts: Vec<_> = rest.split('/').collect();
    if parts.iter().any(|part| part.is_empty()) || parts.len() < 2 {
        bail!("GitHub theme source must be a repository or a .toml file URL");
    }
    let owner = parts[0];
    let repository = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
    validate_github_segment(owner, "owner")?;
    validate_github_segment(repository, "repository")?;
    let canonical_repo = format!("https://github.com/{owner}/{repository}");
    if parts.len() == 2 {
        return Ok(Some(GitHubSource::Repository {
            owner: owner.to_string(),
            repository: repository.to_string(),
            canonical: canonical_repo,
        }));
    }
    if parts.len() >= 5 && parts[2] == "blob" {
        let git_ref = parts[3];
        validate_github_segment(git_ref, "ref")?;
        let path = parts[4..].join("/");
        if !path.ends_with(".toml") {
            bail!("GitHub theme file URL must point to a .toml file");
        }
        return Ok(Some(GitHubSource::File {
            download: format!(
                "https://raw.githubusercontent.com/{owner}/{repository}/{git_ref}/{path}"
            ),
            canonical: format!("{canonical_repo}/blob/{git_ref}/{path}"),
        }));
    }
    bail!("GitHub theme source must be a repository URL or a /blob/<ref>/<path>.toml URL")
}

fn validate_github_segment(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid GitHub {label}");
    }
    Ok(())
}

fn acquire_github_repository(
    owner: &str,
    repository: &str,
    canonical: &str,
) -> Result<(Vec<u8>, String, bool)> {
    let api = format!("{GITHUB_API_ROOT}/{owner}/{repository}/contents");
    let listing = fetch_https_bounded(
        &api,
        MAX_GITHUB_INDEX_BYTES,
        "application/vnd.github+json",
        "GitHub repository listing",
    )?;
    let entries: Vec<GitHubContent> = serde_json::from_slice(&listing).with_context(|| {
        format!("{canonical} must be a public GitHub repository with files at its root")
    })?;
    let selected = select_github_theme(entries)?;
    let download = selected
        .download_url
        .filter(|url| url.starts_with("https://raw.githubusercontent.com/"))
        .ok_or_else(|| anyhow!("GitHub did not provide a safe HTTPS download URL"))?;
    let source = selected
        .html_url
        .filter(|url| url.starts_with("https://github.com/"))
        .unwrap_or_else(|| format!("{canonical}#{}", selected.path));
    Ok((fetch_https(&download)?, source, true))
}

fn select_github_theme(entries: Vec<GitHubContent>) -> Result<GitHubContent> {
    let mut themes: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.kind == "file" && entry.name.ends_with(".toml"))
        .collect();
    match themes.len() {
        0 => bail!("GitHub theme repository must contain one root-level .toml file"),
        1 => {}
        _ => {
            themes.sort_by(|a, b| a.name.cmp(&b.name));
            bail!(
                "GitHub theme repository contains multiple root-level .toml files: {}; install a specific GitHub /blob/ URL instead",
                themes
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let selected = themes.pop().expect("one GitHub theme");
    if selected.size > MAX_FILE_BYTES as u64 {
        bail!("theme file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    Ok(selected)
}

fn fetch_https(url: &str) -> Result<Vec<u8>> {
    fetch_https_bounded(
        url,
        MAX_FILE_BYTES,
        "application/toml,text/plain",
        "theme download",
    )
}

fn fetch_https_bounded(url: &str, limit: usize, accept: &str, label: &str) -> Result<Vec<u8>> {
    if !url.starts_with("https://")
        || url
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("invalid HTTPS URL for {label}");
    }
    let max = limit.to_string();
    let accept_header = format!("Accept: {accept}");
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
        accept_header.as_str(),
        "--header",
        "User-Agent: luvus",
        url,
    ];
    if let Some(bytes) = try_fetch("curl", &curl, limit, label)? {
        return Ok(bytes);
    }
    let quota = format!("--quota={limit}");
    let wget_accept = format!("--header=Accept: {accept}");
    let wget = [
        "-q",
        "-O",
        "-",
        "--timeout=20",
        "--tries=1",
        "--https-only",
        quota.as_str(),
        wget_accept.as_str(),
        "--header=User-Agent: luvus",
        url,
    ];
    if let Some(bytes) = try_fetch("wget", &wget, limit, label)? {
        return Ok(bytes);
    }
    bail!("need curl or wget to download themes")
}

fn try_fetch(program: &str, args: &[&str], limit: usize, label: &str) -> Result<Option<Vec<u8>>> {
    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("run {program}")),
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("capture {program} output"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("capture {program} errors"))?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let mut bytes = Vec::new();
    stdout.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        let _ = errors.join();
        bail!("{label} exceeds the {limit}-byte limit");
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&errors).trim());
    }
    Ok(Some(bytes))
}

fn confirm(file: &ThemeFile, source: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("remote theme installation requires --yes when stdin is not interactive");
    }
    eprintln!("Theme:  {} ({})", file.display_name, file.id);
    eprintln!("Source: {source}");
    eprint!("Install this data-only theme? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("installation cancelled");
    }
    Ok(())
}

fn reject_duplicate_destination(
    registry: &ThemeRegistry,
    id: &str,
    destination: &Path,
) -> Result<()> {
    let Some(entry) = registry.get(id) else {
        return Ok(());
    };
    match &entry.source {
        super::registry::ThemeSource::Local { path, .. } if Path::new(path) == destination => {
            Ok(())
        }
        super::registry::ThemeSource::Local { path, .. } => bail!(
            "theme ID `{id}` is already installed from {path}; remove that file before replacing it"
        ),
        _ => bail!("theme ID `{id}` is reserved by Luvus"),
    }
}

fn restore_file(path: &Path, previous: Option<&[u8]>) {
    match previous {
        Some(bytes) => {
            let _ = atomic_write(path, bytes);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("theme path has no parent"))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("theme"),
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        atomic_replace(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn atomic_replace(temp: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temp, destination)
            .with_context(|| format!("replace {}", destination.display()))?;
    }
    #[cfg(windows)]
    {
        let backup = destination.with_extension("toml.bak");
        if destination.exists() {
            let _ = fs::remove_file(&backup);
            fs::rename(destination, &backup)?;
        }
        if let Err(error) = fs::rename(temp, destination) {
            let _ = fs::rename(&backup, destination);
            return Err(error).with_context(|| format!("replace {}", destination.display()));
        }
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

fn write_provenance(dir: &Path, id: &str, source: &str, bytes: &[u8]) -> Result<()> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let installed_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let encoded = serde_json::to_vec_pretty(&Provenance {
        schema: 1,
        source,
        sha256: digest,
        installed_unix,
    })?;
    atomic_write(&provenance_path(dir, id), &encoded)
}

fn provenance_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.source.json"))
}

fn local_dependents(dir: &Path, parent: &str) -> Vec<String> {
    let mut dependents = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return dependents;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        if let Ok(file) = ThemeFile::parse(&bytes) {
            if file.extends.as_deref() == Some(parent) {
                dependents.push(file.id);
            }
        }
    }
    dependents.sort();
    dependents
}

fn starter_toml(id: &str, extends: Option<&str>) -> String {
    let title = id
        .split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(parent) = extends {
        return format!(
            r##"schema = 1
id = "{id}"
display_name = "{title}"
description = ""
author = ""
version = "1.0.0"
requires_luvus = ">=0.11.0"
appearance = "dark"
extends = "{parent}"

[colors]
# Override only the semantic roles that differ from the parent.
accent = "#c6ff1a"
sel_bg = "#33450e"
"##
        );
    }
    format!(
        r##"schema = 1
id = "{id}"
display_name = "{title}"
description = ""
author = ""
version = "1.0.0"
requires_luvus = ">=0.11.0"
appearance = "dark"

[colors]
crust = "#070709"
mantle = "#111116"
base = "#202028"
surface0 = "#1a1a20"
surface1 = "#25252d"
overlay0 = "#4a4a54"
overlay1 = "#686873"
subtext0 = "#93939f"
subtext1 = "#b6b6c0"
text = "#e7e7ed"
accent = "#c6ff1a"
sel_bg = "#33450e"
border = "#383840"
border_focus = "#8c8c96"
green = "#8fbc7a"
mint = "#6fc6a3"
amber = "#e09a4d"
coral = "#e06c66"
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_coordinate_is_bounded_and_unambiguous() {
        assert_eq!(
            community_url("warm-copper").unwrap(),
            "https://raw.githubusercontent.com/RizRiyz/luvus/main/community/themes/warm-copper.toml"
        );
        for bad in ["../escape", "two words", "a/b"] {
            assert!(community_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn github_repository_and_file_urls_are_unambiguous() {
        assert_eq!(
            parse_github_source("https://github.com/example/theme-repo").unwrap(),
            Some(GitHubSource::Repository {
                owner: "example".into(),
                repository: "theme-repo".into(),
                canonical: "https://github.com/example/theme-repo".into(),
            })
        );
        assert_eq!(
            parse_github_source(
                "https://github.com/example/theme-repo/blob/main/aurora/theme.toml"
            )
            .unwrap(),
            Some(GitHubSource::File {
                download:
                    "https://raw.githubusercontent.com/example/theme-repo/main/aurora/theme.toml"
                        .into(),
                canonical: "https://github.com/example/theme-repo/blob/main/aurora/theme.toml"
                    .into(),
            })
        );
        assert_eq!(
            parse_github_source("https://example.com/theme.toml").unwrap(),
            None
        );
        for bad in [
            "https://github.com/example/theme-repo/issues",
            "https://github.com/example/theme-repo/blob/main/README.md",
            "https://github.com/example/theme-repo?tab=readme",
        ] {
            assert!(parse_github_source(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn github_repository_requires_exactly_one_bounded_root_theme() {
        let entry = |name: &str, kind: &str, size: u64| GitHubContent {
            name: name.into(),
            path: name.into(),
            kind: kind.into(),
            size,
            download_url: Some(format!(
                "https://raw.githubusercontent.com/example/repo/main/{name}"
            )),
            html_url: Some(format!("https://github.com/example/repo/blob/main/{name}")),
        };
        let selected = select_github_theme(vec![
            entry("README.md", "file", 100),
            entry("theme-name.toml", "file", 1_024),
            entry("examples", "dir", 0),
        ])
        .unwrap();
        assert_eq!(selected.name, "theme-name.toml");
        assert!(select_github_theme(vec![entry("README.md", "file", 100)]).is_err());
        assert!(select_github_theme(vec![
            entry("one.toml", "file", 100),
            entry("two.toml", "file", 100),
        ])
        .is_err());
        assert!(select_github_theme(vec![
            entry("large.toml", "file", MAX_FILE_BYTES as u64 + 1,)
        ])
        .is_err());
    }

    #[test]
    fn parent_with_installed_dependents_cannot_be_removed() {
        let _env = crate::persist::test_env("theme-dependent-uninstall");
        let root = crate::persist::ensure_config_dir();
        let parent = root.join("parent.toml");
        init(&parent, "local-parent", None).unwrap();
        install(parent.to_str().unwrap(), true).unwrap();
        let child = root.join("child.toml");
        init(&child, "local-child", Some("local-parent")).unwrap();
        install(child.to_str().unwrap(), true).unwrap();
        let error = uninstall("local-parent").unwrap_err().to_string();
        assert!(error.contains("local-child"), "{error}");
    }

    #[test]
    fn starter_is_valid_schema_one() {
        let complete = starter_toml("my-theme", None);
        ThemeFile::parse(complete.as_bytes())
            .unwrap()
            .colors
            .resolve(None)
            .unwrap();
        let child = starter_toml("my-child", Some("noir"));
        let file = ThemeFile::parse(child.as_bytes()).unwrap();
        assert_eq!(file.extends.as_deref(), Some("noir"));
    }
}
