use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::format::{Appearance, ThemeFile, MAX_FILE_BYTES, MAX_INHERITANCE_DEPTH};
use crate::ui::theme::{self, Theme};

const MAX_THEME_FILES: usize = 256;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSource {
    BuiltIn,
    Local {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_from: Option<String>,
    },
    Virtual,
}

#[derive(Clone, Debug)]
pub struct ThemeEntry {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub author: String,
    pub license: String,
    pub version: String,
    pub appearance: Appearance,
    pub source: ThemeSource,
    pub extends: Option<String>,
    pub theme: Theme,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ThemeProblem {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ThemeRegistry {
    entries: Vec<ThemeEntry>,
    by_id: HashMap<String, usize>,
    problems: Vec<ThemeProblem>,
}

#[derive(Clone)]
struct Candidate {
    file: ThemeFile,
    path: PathBuf,
    digest: String,
}

impl ThemeRegistry {
    pub fn load() -> Self {
        Self::load_from(&super::themes_dir())
    }

    pub fn load_from(dir: &Path) -> Self {
        let mut registry = Self::builtins();
        let mut candidates = HashMap::<String, Candidate>::new();
        let mut duplicate_ids = HashSet::new();
        let mut paths = Vec::new();

        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_toml = path.extension().and_then(|value| value.to_str()) == Some("toml");
                    let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
                    if is_toml && is_file {
                        paths.push(path);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => registry.problems.push(ThemeProblem {
                path: dir.display().to_string(),
                message: format!("cannot read themes directory: {error}"),
            }),
        }
        paths.sort();
        if paths.len() > MAX_THEME_FILES {
            registry.problems.push(ThemeProblem {
                path: dir.display().to_string(),
                message: format!(
                    "theme directory has {} files; only the first {MAX_THEME_FILES} are loaded",
                    paths.len()
                ),
            });
            paths.truncate(MAX_THEME_FILES);
        }

        for path in paths {
            match read_theme_file(&path) {
                Ok((file, _)) if is_reserved_id(&file.id) => registry.problems.push(ThemeProblem {
                    path: path.display().to_string(),
                    message: format!("theme ID `{}` is reserved by Luvus", file.id),
                }),
                Ok((file, digest)) => {
                    let id = file.id.clone();
                    if candidates.contains_key(&id) {
                        duplicate_ids.insert(id.clone());
                    }
                    candidates.insert(id, Candidate { file, path, digest });
                }
                Err(error) => registry.problems.push(ThemeProblem {
                    path: path.display().to_string(),
                    message: format!("{error:#}"),
                }),
            }
        }

        for id in duplicate_ids {
            candidates.remove(&id);
            registry.problems.push(ThemeProblem {
                path: dir.display().to_string(),
                message: format!("duplicate local theme ID `{id}`"),
            });
        }

        let builtin_themes: HashMap<String, Theme> = registry
            .entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.theme.clone()))
            .collect();
        let mut resolved = HashMap::<String, (Theme, usize)>::new();
        let mut failed = HashMap::<String, String>::new();
        let ids: Vec<String> = candidates.keys().cloned().collect();
        for id in ids {
            let mut stack = Vec::new();
            if let Err(error) = resolve_candidate(
                &id,
                &candidates,
                &builtin_themes,
                &mut resolved,
                &mut failed,
                &mut stack,
            ) {
                failed.insert(id, format!("{error:#}"));
            }
        }

        let mut local_entries = Vec::new();
        for (id, candidate) in candidates {
            if let Some(message) = failed.get(&id) {
                registry.problems.push(ThemeProblem {
                    path: candidate.path.display().to_string(),
                    message: message.clone(),
                });
                continue;
            }
            let Some((theme, _)) = resolved.remove(&id) else {
                continue;
            };
            let mut warnings = candidate.file.warnings(&theme);
            let (installed_from, provenance_warning) =
                provenance(&candidate.path, &candidate.digest);
            if let Some(warning) = provenance_warning {
                warnings.push(warning);
            }
            local_entries.push(ThemeEntry {
                id: candidate.file.id,
                display_name: candidate.file.display_name,
                description: candidate.file.description,
                author: candidate.file.author,
                license: candidate.file.license,
                version: candidate.file.version,
                appearance: candidate.file.appearance,
                source: ThemeSource::Local {
                    path: candidate.path.display().to_string(),
                    installed_from,
                },
                extends: candidate.file.extends,
                theme,
                warnings,
            });
        }
        local_entries.sort_by(|a, b| a.id.cmp(&b.id));

        let terminal_index = registry
            .entries
            .iter()
            .position(|entry| entry.id == "terminal")
            .expect("terminal built-in entry");
        let terminal = registry.entries.remove(terminal_index);
        registry.entries.extend(local_entries);
        registry.entries.push(terminal);
        registry.reindex();
        registry
    }

    pub fn entries(&self) -> &[ThemeEntry] {
        &self.entries
    }

    pub fn problems(&self) -> &[ThemeProblem] {
        &self.problems
    }

    pub fn get(&self, id: &str) -> Option<&ThemeEntry> {
        let canonical = theme::canonical(id);
        self.by_id
            .get(canonical)
            .and_then(|index| self.entries.get(*index))
    }

    pub fn theme(&self, id: &str) -> Option<Theme> {
        self.get(id).map(|entry| entry.theme.clone())
    }

    pub fn theme_or_default(&self, id: &str) -> Theme {
        self.theme(id).unwrap_or_else(Theme::quattro_rally)
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.by_id.get(theme::canonical(id)).copied()
    }

    pub fn list_json(&self, active: &str) -> serde_json::Value {
        let themes: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "display_name": entry.display_name,
                    "description": entry.description,
                    "author": entry.author,
                    "license": entry.license,
                    "version": entry.version,
                    "appearance": format!("{:?}", entry.appearance).to_ascii_lowercase(),
                    "source": entry.source,
                    "extends": entry.extends,
                    "warnings": entry.warnings,
                    "active": theme::canonical(active) == entry.id,
                })
            })
            .collect();
        serde_json::json!({"themes": themes, "problems": self.problems})
    }

    fn builtins() -> Self {
        let mut entries = Vec::new();
        for id in theme::THEMES {
            let virtual_theme = *id == "terminal";
            entries.push(ThemeEntry {
                id: (*id).to_string(),
                display_name: display_name(id),
                description: theme::describe(id).to_string(),
                author: "Luvus".to_string(),
                license: String::new(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                appearance: if virtual_theme {
                    Appearance::Terminal
                } else if matches!(*id, "sky" | "catppuccin-latte" | "gruvbox-light") {
                    Appearance::Light
                } else {
                    Appearance::Dark
                },
                source: if virtual_theme {
                    ThemeSource::Virtual
                } else {
                    ThemeSource::BuiltIn
                },
                extends: None,
                theme: theme::by_name(id),
                warnings: Vec::new(),
            });
        }
        let mut registry = Self {
            entries,
            by_id: HashMap::new(),
            problems: Vec::new(),
        };
        registry.reindex();
        registry
    }

    fn reindex(&mut self) {
        self.by_id = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
    }
}

pub fn is_reserved_id(id: &str) -> bool {
    theme::THEMES.contains(&id) || theme::canonical(id) != id
}

pub fn validate_standalone(
    file: &ThemeFile,
    registry: &ThemeRegistry,
) -> Result<(Theme, Vec<String>)> {
    if is_reserved_id(&file.id) {
        bail!("theme ID `{}` is reserved by Luvus", file.id);
    }
    let parent = match file.extends.as_deref() {
        Some(id) => Some(
            registry
                .get(id)
                .with_context(|| format!("parent theme `{id}` is not installed"))?
                .theme
                .clone(),
        ),
        None => None,
    };
    let resolved = file.colors.resolve(parent.as_ref())?;
    let warnings = file.warnings(&resolved);
    Ok((resolved, warnings))
}

fn read_theme_file(path: &Path) -> Result<(ThemeFile, String)> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        bail!("theme file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((ThemeFile::parse(&bytes)?, digest))
}

fn provenance(path: &Path, digest: &str) -> (Option<String>, Option<String>) {
    let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
        return (None, None);
    };
    let sidecar = path.with_file_name(format!("{id}.source.json"));
    let bytes = match fs::read(&sidecar) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(error) => {
            return (
                None,
                Some(format!("cannot read install provenance: {error}")),
            )
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => return (None, Some(format!("invalid install provenance: {error}"))),
    };
    let source = value
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let warning = match value.get("sha256").and_then(serde_json::Value::as_str) {
        Some(expected) if expected == digest => None,
        Some(_) => Some(
            "installed file differs from its recorded SHA-256 digest (manual edit or corruption)"
                .to_string(),
        ),
        None => Some("install provenance has no SHA-256 digest".to_string()),
    };
    (source, warning)
}

fn resolve_candidate(
    id: &str,
    candidates: &HashMap<String, Candidate>,
    builtins: &HashMap<String, Theme>,
    resolved: &mut HashMap<String, (Theme, usize)>,
    failed: &mut HashMap<String, String>,
    stack: &mut Vec<String>,
) -> Result<(Theme, usize)> {
    if let Some(resolved) = resolved.get(id) {
        return Ok(resolved.clone());
    }
    if let Some(message) = failed.get(id) {
        bail!("{message}");
    }
    if stack.len() >= MAX_INHERITANCE_DEPTH {
        bail!("theme inheritance exceeds {MAX_INHERITANCE_DEPTH} levels");
    }
    if stack.iter().any(|entry| entry == id) {
        stack.push(id.to_string());
        bail!("theme inheritance cycle: {}", stack.join(" -> "));
    }
    let candidate = candidates
        .get(id)
        .with_context(|| format!("theme `{id}` is unavailable"))?;
    stack.push(id.to_string());
    let parent = match candidate.file.extends.as_deref() {
        Some(parent) => {
            let canonical = theme::canonical(parent);
            if let Some(theme) = builtins.get(canonical) {
                Some((theme.clone(), 0))
            } else if candidates.contains_key(parent) {
                Some(resolve_candidate(
                    parent, candidates, builtins, resolved, failed, stack,
                )?)
            } else {
                bail!("parent theme `{parent}` is not installed");
            }
        }
        None => None,
    };
    stack.pop();
    let depth = parent.as_ref().map_or(1, |(_, depth)| depth + 1);
    if depth > MAX_INHERITANCE_DEPTH {
        bail!("theme inheritance exceeds {MAX_INHERITANCE_DEPTH} levels");
    }
    let theme = candidate
        .file
        .colors
        .resolve(parent.as_ref().map(|(theme, _)| theme))?;
    resolved.insert(id.to_string(), (theme.clone(), depth));
    Ok((theme, depth))
}

fn display_name(id: &str) -> String {
    id.split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    fn complete(id: &str) -> String {
        format!(
            r##"schema = 1
id = "{id}"
display_name = "{id}"
appearance = "dark"
[colors]
crust = "#000000"
mantle = "#101010"
base = "#202020"
surface0 = "#303030"
surface1 = "#404040"
overlay0 = "#505050"
overlay1 = "#606060"
subtext0 = "#a0a0a0"
subtext1 = "#c0c0c0"
text = "#ffffff"
accent = "#00ff00"
sel_bg = "#203020"
border = "#505050"
border_focus = "#909090"
green = "#00aa00"
mint = "#00aaaa"
amber = "#aaaa00"
coral = "#aa0000"
"##
        )
    }

    fn temp(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "luvus-theme-registry-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn loads_custom_themes_between_builtins_and_terminal() {
        let dir = temp("load");
        write(&dir, "z.toml", &complete("z-custom"));
        write(&dir, "a.toml", &complete("a-custom"));
        let registry = ThemeRegistry::load_from(&dir);
        let ids: Vec<_> = registry
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids.last(), Some(&"terminal"));
        assert!(registry.index_of("a-custom").unwrap() < registry.index_of("z-custom").unwrap());
        assert!(registry.problems().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolves_inheritance_and_rejects_cycles_and_reserved_ids() {
        let dir = temp("inherit");
        write(
            &dir,
            "child.toml",
            r##"schema = 1
id = "child"
display_name = "Child"
extends = "noir"
[colors]
accent = "#123456"
"##,
        );
        write(
            &dir,
            "cycle-a.toml",
            r##"schema = 1
id = "cycle-a"
display_name = "A"
extends = "cycle-b"
[colors]
accent = "#111111"
"##,
        );
        write(
            &dir,
            "cycle-b.toml",
            r##"schema = 1
id = "cycle-b"
display_name = "B"
extends = "cycle-a"
[colors]
accent = "#222222"
"##,
        );
        write(&dir, "noir.toml", &complete("noir"));
        let registry = ThemeRegistry::load_from(&dir);
        assert_eq!(
            registry.theme("child").unwrap().accent,
            ratatui::style::Color::Rgb(0x12, 0x34, 0x56)
        );
        assert!(registry.get("cycle-a").is_none());
        assert!(registry
            .problems()
            .iter()
            .any(|problem| problem.message.contains("cycle")));
        assert!(registry
            .problems()
            .iter()
            .any(|problem| problem.message.contains("reserved")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn aliases_still_select_their_builtins() {
        let registry = ThemeRegistry::load_from(Path::new("/path/that/does/not/exist"));
        assert_eq!(
            registry.index_of("mocha"),
            registry.index_of("catppuccin-mocha")
        );
    }

    #[test]
    fn missing_duplicate_and_overdeep_inheritance_are_omitted() {
        let dir = temp("invalid-graphs");
        write(
            &dir,
            "missing.toml",
            r##"schema = 1
id = "missing-parent"
display_name = "Missing"
extends = "does-not-exist"
[colors]
accent = "#123456"
"##,
        );
        write(&dir, "duplicate-a.toml", &complete("duplicate"));
        write(&dir, "duplicate-b.toml", &complete("duplicate"));
        for index in 0..=8 {
            let parent = if index == 0 {
                "noir".to_string()
            } else {
                format!("deep-{}", index - 1)
            };
            write(
                &dir,
                &format!("deep-{index}.toml"),
                &format!(
                    r##"schema = 1
id = "deep-{index}"
display_name = "Deep {index}"
extends = "{parent}"
[colors]
accent = "#123456"
"##
                ),
            );
        }
        let registry = ThemeRegistry::load_from(&dir);
        assert!(registry.get("missing-parent").is_none());
        assert!(registry.get("duplicate").is_none());
        assert!(registry.get("deep-8").is_none());
        let messages = registry
            .problems()
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(messages.contains("not installed"), "{messages}");
        assert!(messages.contains("duplicate"), "{messages}");
        assert!(messages.contains("exceeds"), "{messages}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn provenance_digest_mismatch_is_reported_without_disabling_manual_edits() {
        let dir = temp("provenance");
        write(&dir, "managed.toml", &complete("managed"));
        fs::write(
            dir.join("managed.source.json"),
            r#"{"schema":1,"source":"https://example.com/managed.toml","sha256":"bad"}"#,
        )
        .unwrap();
        let registry = ThemeRegistry::load_from(&dir);
        let entry = registry.get("managed").expect("theme remains usable");
        assert!(entry
            .warnings
            .iter()
            .any(|warning| warning.contains("SHA-256")));
        match &entry.source {
            ThemeSource::Local { installed_from, .. } => assert_eq!(
                installed_from.as_deref(),
                Some("https://example.com/managed.toml")
            ),
            source => panic!("unexpected source: {source:?}"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn community_theme_files_are_valid() {
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("community/themes");
        let dir = std::env::temp_dir().join(format!(
            "luvus-community-theme-validation-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The registry targets the next public release while Cargo.toml remains
        // at the last released version until the release workflow bumps it. Keep
        // the checked-in >=0.12 contract, but substitute this test binary's version
        // only while exercising schema resolution and website drift checks.
        for item in fs::read_dir(&source_dir).unwrap().flatten() {
            let path = item.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            assert!(
                source.contains("requires_luvus = \">=0.12.0\""),
                "{} must require Luvus 0.12 or newer",
                path.display()
            );
            assert!(
                !source.contains("\nlicense ="),
                "{} should use the repository license",
                path.display()
            );
            let compatible = source.replace(
                "requires_luvus = \">=0.12.0\"",
                concat!("requires_luvus = \">=", env!("CARGO_PKG_VERSION"), "\""),
            );
            fs::write(dir.join(path.file_name().unwrap()), compatible).unwrap();
        }
        let registry = ThemeRegistry::load_from(&dir);
        assert!(
            registry.problems().is_empty(),
            "community registry problems: {:?}",
            registry.problems()
        );
        let local: Vec<_> = registry
            .entries()
            .iter()
            .filter(|entry| matches!(entry.source, ThemeSource::Local { .. }))
            .collect();
        assert_eq!(
            local.len(),
            12,
            "all reviewed starter themes are registered"
        );
        let website = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("website/src/lib/theme-schema.ts"),
        )
        .unwrap();
        for entry in local {
            let marker = format!("id: '{}'", entry.id);
            let after = website
                .split_once(&marker)
                .unwrap_or_else(|| panic!("{} is missing from the /themes gallery", entry.id))
                .1;
            let block = after.split("\n  {\n    id: '").next().unwrap_or(after);
            let colors = [
                ("crust", entry.theme.crust),
                ("mantle", entry.theme.mantle),
                ("base", entry.theme.base),
                ("surface0", entry.theme.surface0),
                ("surface1", entry.theme.surface1),
                ("overlay0", entry.theme.overlay0),
                ("overlay1", entry.theme.overlay1),
                ("subtext0", entry.theme.subtext0),
                ("subtext1", entry.theme.subtext1),
                ("text", entry.theme.text),
                ("accent", entry.theme.accent),
                ("sel_bg", entry.theme.sel_bg),
                ("border", entry.theme.border),
                ("border_focus", entry.theme.border_focus),
                ("green", entry.theme.green),
                ("mint", entry.theme.mint),
                ("amber", entry.theme.amber),
                ("coral", entry.theme.coral),
            ];
            for (field, color) in colors {
                let ratatui::style::Color::Rgb(r, g, b) = color else {
                    panic!("community starter {} {field} is not RGB", entry.id);
                };
                assert!(
                    block.contains(&format!("{field}: '#{r:02x}{g:02x}{b:02x}'")),
                    "{} {field} differs between community TOML and /themes",
                    entry.id
                );
            }
            assert!(
                entry.warnings.is_empty(),
                "{} has strict warnings: {:?}",
                entry.id,
                entry.warnings
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
