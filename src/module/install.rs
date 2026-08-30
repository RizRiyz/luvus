//! `luvus module install owner/repo[/sub]` (docs/13 MOD-4): shallow-clone a git
//! source into a staging dir, **preview every command + confirm**, run
//! `[[build]]` with a scrubbed env, verify the manifest didn't change, then
//! atomically move it into the managed modules dir with a pinned commit. The CLI
//! registers the result over the socket. Rolls back the staging dir on any error.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use super::manifest::{ModuleManifest, MANIFEST_FILE};
use super::paths;

pub struct Installed {
    pub root: PathBuf,
    pub source: String,
    pub id: String,
}

/// Clone, build, and stage a module; returns where it landed + its pinned source.
pub fn install(spec: &str, git_ref: Option<&str>, yes: bool) -> Result<Installed> {
    let (url, slug, sub) = parse_spec(spec)?;
    let staging = paths::staging_dir();
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent).context("create managed modules dir")?;
    }
    let result = install_inner(&url, &slug, &sub, git_ref, yes, &staging, spec);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// Uninstall guard helper: a path is only deletable by `module uninstall` if
/// it's a managed git checkout.
pub fn is_removable(root: &Path) -> bool {
    paths::is_managed_git_path(root)
}

fn install_inner(
    url: &str,
    slug: &str,
    sub: &str,
    git_ref: Option<&str>,
    yes: bool,
    staging: &Path,
    spec: &str,
) -> Result<Installed> {
    // 1. Shallow clone.
    git(&["clone", "--depth", "1", url, &staging.to_string_lossy()])
        .with_context(|| format!("git clone {url}"))?;
    if let Some(r) = git_ref {
        git(&[
            "-C",
            &staging.to_string_lossy(),
            "fetch",
            "--depth",
            "1",
            "origin",
            r,
        ])
        .with_context(|| format!("fetch ref {r}"))?;
        git(&["-C", &staging.to_string_lossy(), "checkout", "FETCH_HEAD"])
            .with_context(|| format!("checkout ref {r}"))?;
    }

    let module_root = if sub.is_empty() {
        staging.to_path_buf()
    } else {
        staging.join(sub)
    };

    // 2. Load + validate the manifest, and snapshot it for the immutability check.
    let manifest_path = ModuleManifest::path(&module_root);
    let before = fs::read(&manifest_path)
        .with_context(|| format!("no {MANIFEST_FILE} at {}", module_root.display()))?;
    let manifest = ModuleManifest::load(&module_root).map_err(|e| anyhow!("{e}"))?;
    let sha = git_capture(&["-C", &staging.to_string_lossy(), "rev-parse", "HEAD"])?;
    let sha = sha.trim().to_string();

    // 3. Preview + confirm.
    print_preview(spec, &sha, &manifest);
    if !yes && !confirm()? {
        bail!("aborted");
    }

    // 4. Run [[build]] with a scrubbed environment (no LUVUS_*/socket access).
    // A step gated to other platforms is skipped, so one manifest can carry
    // both a `npm.cmd` step for Windows and a `make` step for Unix.
    for b in &manifest.build {
        if !super::manifest::allowed_on(b.platforms.as_ref()) {
            continue;
        }
        run_build(&module_root, &b.command)
            .with_context(|| format!("build step {:?} failed", b.command))?;
    }

    // 5. The selected manifest must not have changed during the build. Resolve
    // it again because a build that started from the legacy filename could add
    // a higher-priority luvus-module.toml without touching the checked bytes.
    verify_manifest_unchanged(&module_root, &manifest_path, &before)?;

    // 6. Atomically move into the managed dir.
    let dest = paths::git_dir(slug, &short(&sha));
    if dest.exists() {
        let _ = fs::remove_dir_all(&dest);
    }
    fs::rename(&module_root, &dest).with_context(|| format!("move into {}", dest.display()))?;
    // If we moved a subdir out, drop the rest of the clone.
    if !sub.is_empty() {
        let _ = fs::remove_dir_all(staging);
    }

    Ok(Installed {
        root: dest,
        source: format!("{spec}@{sha}"),
        id: manifest.id,
    })
}

fn verify_manifest_unchanged(root: &Path, before_path: &Path, before: &[u8]) -> Result<()> {
    let after_path = ModuleManifest::path(root);
    let after = fs::read(&after_path).context("re-read manifest after build")?;
    if after_path != before_path || after != before {
        bail!("manifest changed during build — refusing to install");
    }
    Ok(())
}

/// Parse `owner/repo[/sub]` → a GitHub URL, or a local path / git URL verbatim.
fn parse_spec(spec: &str) -> Result<(String, String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("usage: luvus module install owner/repo[/sub]");
    }
    // A path or explicit URL is cloned as-is, with no subdirectory.
    let is_url_or_path = spec.contains("://")
        || spec.starts_with('/')
        || spec.starts_with('.')
        || spec.starts_with('~');
    if is_url_or_path {
        let slug = Path::new(spec.trim_end_matches('/'))
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("module")
            .trim_end_matches(".git")
            .to_string();
        return Ok((spec.to_string(), slug, String::new()));
    }
    let parts: Vec<&str> = spec.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        bail!("expected owner/repo[/sub], got {spec:?}");
    }
    let (owner, repo) = (parts[0], parts[1]);
    let sub = parts[2..].join("/");
    let url = format!("https://github.com/{owner}/{repo}.git");
    Ok((url, format!("{owner}-{repo}"), sub))
}

fn print_preview(spec: &str, sha: &str, m: &ModuleManifest) {
    println!("Install module from {spec}");
    println!("  id:      {}", m.id);
    println!("  name:    {} {}", m.name, m.version);
    println!("  commit:  {}", short(sha));
    let mut commands: Vec<String> = Vec::new();
    for b in &m.build {
        commands.push(format!("  build:   {}", b.command.join(" ")));
    }
    for a in &m.actions {
        commands.push(format!("  action {}: {}", a.id, a.command.join(" ")));
    }
    for p in &m.panes {
        commands.push(format!("  pane {}: {}", p.id, p.command.join(" ")));
    }
    for e in &m.events {
        commands.push(format!("  on {}: {}", e.on, e.command.join(" ")));
    }
    if commands.is_empty() {
        println!("  (no commands declared)");
    } else {
        println!("Commands this module can run:");
        for c in commands {
            println!("{c}");
        }
    }
}

fn confirm() -> Result<bool> {
    print!("Proceed? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Run a build command in `dir` with a scrubbed environment — no `LUVUS_*` and
/// no socket access, so build steps can't drive luvus.
fn run_build(dir: &Path, argv: &[String]) -> Result<()> {
    let Some((program, args)) = argv.split_first() else {
        bail!("empty build command");
    };
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir);
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if !k.starts_with("LUVUS_") {
            cmd.env(k, v);
        }
    }
    let status = cmd.status().with_context(|| format!("spawn {program}"))?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

fn git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .context("running git (is it installed?)")?;
    if !status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(())
}

fn git_capture(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo_and_paths() {
        let (url, slug, sub) = parse_spec("you/git-status").unwrap();
        assert_eq!(url, "https://github.com/you/git-status.git");
        assert_eq!(slug, "you-git-status");
        assert!(sub.is_empty());

        let (_, _, sub) = parse_spec("you/repo/modules/board").unwrap();
        assert_eq!(sub, "modules/board");

        let (url, slug, sub) = parse_spec("/tmp/local-mod").unwrap();
        assert_eq!(url, "/tmp/local-mod");
        assert_eq!(slug, "local-mod");
        assert!(sub.is_empty());

        assert!(parse_spec("nope").is_err());
    }

    #[test]
    fn manifest_selection_cannot_change_during_build() {
        let root =
            std::env::temp_dir().join(format!("luvus-manifest-switch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let manifest_path = root.join(MANIFEST_FILE);
        let before = b"module manifest";
        fs::write(&manifest_path, before).unwrap();
        assert_eq!(ModuleManifest::path(&root), manifest_path);

        fs::write(&manifest_path, "replacement manifest").unwrap();
        let error = verify_manifest_unchanged(&root, &manifest_path, before)
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest changed during build"), "{error}");
        let _ = fs::remove_dir_all(root);
    }
}
