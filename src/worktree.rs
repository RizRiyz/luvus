//! Worktree creation provider boundary.
//!
//! The built-in Git provider remains the default. A module can opt into the
//! creation boundary with `[worktree_provider]`; Luvus runs only that fixed
//! manifest argv and validates the returned checkout before opening it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::WorktreeConfig;
use crate::module::ModuleRegistry;

#[derive(Deserialize)]
struct ProviderOutput {
    path: PathBuf,
}

pub struct CreateRequest {
    pub repo: PathBuf,
    pub branch: String,
}

pub struct RemoveRequest {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub force: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Create,
    Remove,
}

pub enum ProviderOperation {
    Create(CreateRequest),
    Remove(RemoveRequest),
}

struct ProviderCommand {
    module: crate::module::InstalledModule,
    token: String,
    context: Value,
    argv: Vec<String>,
}

pub struct ProviderJob {
    command: ProviderCommand,
    operation: ProviderOperation,
}

pub enum ProviderResult {
    Created(CreatedWorktree),
    Removed(PathBuf),
}

impl ProviderJob {
    pub fn kind(&self) -> ProviderKind {
        match &self.operation {
            ProviderOperation::Create(_) => ProviderKind::Create,
            ProviderOperation::Remove(_) => ProviderKind::Remove,
        }
    }

    pub fn run(self, cancelled: &std::sync::atomic::AtomicBool) -> Result<ProviderResult, String> {
        match self.operation {
            ProviderOperation::Create(request) => run_create(&self.command, request, cancelled),
            ProviderOperation::Remove(request) => run_remove(&self.command, request, cancelled),
        }
    }
}

impl ProviderCommand {
    fn run(
        &self,
        request: Value,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<String, String> {
        crate::module::runtime::run_sync(
            &self.module,
            &self.token,
            &self.context,
            &self.argv,
            &request,
            cancelled,
        )
    }
}

fn run_create(
    command: &ProviderCommand,
    request: CreateRequest,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<ProviderResult, String> {
    let branch_exists = bounded_branch_exists(&request.repo, &request.branch, cancelled)?;
    let existing_worktrees = bounded_worktree_paths(&request.repo, cancelled)?;
    let stdout = command.run(
        json!({
            "version": 1,
            "operation": "create",
            "repository": request.repo.display().to_string(),
            "branch": request.branch,
            "branch_exists": branch_exists,
        }),
        cancelled,
    )?;
    let path = parse_provider_output(stdout.as_bytes())?;
    if let Err(error) = validate_bounded(&request.repo, &path, &request.branch, cancelled) {
        cleanup_failed_creation(
            &request.repo,
            &path,
            &request.branch,
            !branch_exists,
            &existing_worktrees,
        );
        return Err(error);
    }
    let worktree_created = !worktree_existed(&path, &existing_worktrees);
    Ok(ProviderResult::Created(CreatedWorktree {
        repo: request.repo,
        path,
        branch: request.branch,
        worktree_created,
        branch_created: !branch_exists,
    }))
}

fn run_remove(
    command: &ProviderCommand,
    request: RemoveRequest,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<ProviderResult, String> {
    let canonical_path = validate_remove_target(&request.repo, &request.path, cancelled)?;
    let stdout = command.run(
        json!({
            "version": 1,
            "operation": "remove",
            "repository": request.repo.display().to_string(),
            "path": request.path.display().to_string(),
            "branch": request.branch,
            "force": request.force,
        }),
        cancelled,
    )?;
    if !stdout.trim().is_empty() {
        return Err("worktree removal provider must not write to stdout".into());
    }
    validate_removed(&request.repo, &request.path, &canonical_path, cancelled)?;
    Ok(ProviderResult::Removed(request.path))
}

pub fn module_remove_job(
    config: &WorktreeConfig,
    modules: &ModuleRegistry,
    module_tokens: &HashMap<String, String>,
    context: Value,
    request: RemoveRequest,
) -> Result<Option<ProviderJob>, String> {
    let provider_id = config.provider.trim();
    if provider_id.eq_ignore_ascii_case("git") {
        return Ok(None);
    }
    let (module, provider) = resolve_module(modules, provider_id)?;
    let Some(argv) = provider.remove_command.as_ref() else {
        return Ok(None);
    };
    let token = module_tokens
        .get(&module.id)
        .ok_or_else(|| format!("module {provider_id} has no runtime credential"))?;
    Ok(Some(ProviderJob {
        command: ProviderCommand {
            module: module.clone(),
            token: token.clone(),
            context,
            argv: argv.clone(),
        },
        operation: ProviderOperation::Remove(request),
    }))
}

pub struct CreatedWorktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub worktree_created: bool,
    pub branch_created: bool,
}

impl CreatedWorktree {
    pub fn matches(&self, repo: &Path, branch: &str) -> bool {
        crate::platform::same_path(&self.repo, repo) && self.branch == branch
    }
}

pub fn module_job(
    config: &WorktreeConfig,
    modules: &ModuleRegistry,
    module_tokens: &HashMap<String, String>,
    context: Value,
    repo: &Path,
    branch: &str,
) -> Result<Option<ProviderJob>, String> {
    let provider_id = config.provider.trim();
    if provider_id.eq_ignore_ascii_case("git") {
        return Ok(None);
    }
    let (module, provider) = resolve_module(modules, provider_id)?;
    let token = module_tokens
        .get(&module.id)
        .ok_or_else(|| format!("module {provider_id} has no runtime credential"))?;
    Ok(Some(ProviderJob {
        command: ProviderCommand {
            module: module.clone(),
            token: token.clone(),
            context,
            argv: provider.command.clone(),
        },
        operation: ProviderOperation::Create(CreateRequest {
            repo: repo.to_path_buf(),
            branch: branch.to_string(),
        }),
    }))
}

/// Create `branch` with the built-in Git provider and verify the checkout.
pub fn create_git(repo: &Path, git_path: &Path, branch: &str) -> Result<PathBuf, String> {
    crate::git::local::worktree_add(repo, git_path, branch)?;
    validate(repo, git_path, branch)?;
    Ok(git_path.to_path_buf())
}

pub(crate) fn validate_config(
    config: &WorktreeConfig,
    modules: Option<&ModuleRegistry>,
) -> Result<(), String> {
    let provider = config.provider.trim();
    if provider.is_empty() {
        return Err("worktree.provider must not be empty".to_string());
    }
    if provider.eq_ignore_ascii_case("git") {
        return Ok(());
    }
    if let Some(modules) = modules {
        resolve_module(modules, provider)?;
    }
    Ok(())
}

fn resolve_module<'a>(
    modules: &'a ModuleRegistry,
    provider_id: &str,
) -> Result<
    (
        &'a crate::module::InstalledModule,
        &'a crate::module::manifest::WorktreeProvider,
    ),
    String,
> {
    let module = modules
        .find(provider_id)
        .ok_or_else(|| format!("worktree provider module is not installed: {provider_id}"))?;
    if module.id != provider_id {
        return Err(format!(
            "worktree.provider must use the canonical module id: {}",
            module.id
        ));
    }
    if !module.is_runnable() {
        return Err(format!(
            "worktree provider module is not enabled: {provider_id}"
        ));
    }
    let provider = module
        .manifest
        .worktree_provider()
        .ok_or_else(|| format!("module {provider_id} does not declare [worktree_provider]"))?;
    Ok((module, provider))
}

fn validate_remove_target(
    repo: &Path,
    path: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<PathBuf, String> {
    let worktrees = bounded_worktree_paths(repo, cancelled)?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve worktree {}: {error}", path.display()))?;
    let position = worktrees.iter().position(|candidate| {
        std::fs::canonicalize(candidate)
            .map(|candidate| crate::platform::same_path(&candidate, &canonical))
            .unwrap_or(false)
    });
    match position {
        None => return Err("worktree removal target is not registered".into()),
        Some(0) => return Err("the main worktree cannot be removed".into()),
        Some(_) => {}
    }
    let source_common = bounded_common_dir(repo, cancelled, std::time::Duration::from_secs(300))?;
    let target_common = bounded_common_dir(path, cancelled, std::time::Duration::from_secs(300))?;
    if !crate::platform::same_path(&source_common, &target_common) {
        return Err("worktree removal target belongs to another repository".into());
    }
    Ok(canonical)
}

fn validate_removed(
    repo: &Path,
    path: &Path,
    canonical_path: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "worktree removal provider left the directory in place: {}",
            path.display()
        ));
    }
    let registered = bounded_worktree_paths(repo, cancelled)?
        .iter()
        .any(|candidate| {
            std::fs::canonicalize(candidate)
                .map(|candidate| crate::platform::same_path(&candidate, canonical_path))
                .unwrap_or_else(|_| {
                    crate::platform::same_path(candidate, canonical_path)
                        || crate::platform::same_path(candidate, path)
                })
        });
    if registered {
        return Err("worktree removal provider left the worktree registered".into());
    }
    Ok(())
}

fn parse_provider_output(stdout: &[u8]) -> Result<PathBuf, String> {
    let output: ProviderOutput = serde_json::from_slice(stdout)
        .map_err(|error| format!("worktree provider returned invalid JSON: {error}"))?;
    if !output.path.is_absolute() {
        return Err("worktree provider returned a non-absolute path".to_string());
    }
    Ok(output.path)
}

fn bounded_git(
    cwd: &Path,
    args: &[&str],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    crate::module::runtime::run_bounded_argv(&cwd.to_path_buf(), &argv, cancelled)
}

fn bounded_worktree_paths(
    repo: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Vec<PathBuf>, String> {
    let raw = bounded_git(repo, &["worktree", "list", "--porcelain", "-z"], cancelled)
        .or_else(|_| bounded_git(repo, &["worktree", "list", "--porcelain"], cancelled))?;
    Ok(raw
        .split(['\0', '\n'])
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn bounded_branch_exists(
    repo: &Path,
    branch: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<bool, String> {
    let refname = format!("refs/heads/{branch}");
    match bounded_git(
        repo,
        &["show-ref", "--verify", "--quiet", &refname],
        cancelled,
    ) {
        Ok(_) => Ok(true),
        Err(error) if error == "command failed" => Ok(false),
        Err(error) => Err(error),
    }
}

fn worktree_existed(path: &Path, existing_worktrees: &[PathBuf]) -> bool {
    let Some(returned) = std::fs::canonicalize(path).ok() else {
        return false;
    };
    existing_worktrees.iter().any(|existing| {
        std::fs::canonicalize(existing)
            .map(|existing| crate::platform::same_path(&existing, &returned))
            .unwrap_or(false)
    })
}

fn cleanup_failed_creation(
    repo: &Path,
    path: &Path,
    branch: &str,
    branch_created: bool,
    existing_worktrees: &[PathBuf],
) {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let cleanup = AtomicBool::new(false);
    if worktree_existed(path, existing_worktrees) {
        return;
    }
    let source_common = bounded_common_dir(repo, &cleanup, Duration::from_secs(5));
    let returned_common = bounded_common_dir(path, &cleanup, Duration::from_secs(5));
    if source_common.is_err()
        || returned_common.is_err()
        || !crate::platform::same_path(&source_common.unwrap(), &returned_common.unwrap())
    {
        return;
    }
    let run = |args: Vec<String>| {
        crate::module::runtime::run_bounded_argv_for(
            &repo.to_path_buf(),
            &args,
            &cleanup,
            Duration::from_secs(5),
        )
    };
    let removed = run(vec![
        "git".into(),
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        path.display().to_string(),
    ])
    .is_ok();
    if removed && branch_created {
        let _ = run(vec![
            "git".into(),
            "branch".into(),
            "-D".into(),
            "--".into(),
            branch.to_string(),
        ]);
    }
}

fn bounded_common_dir(
    cwd: &Path,
    cancelled: &std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
) -> Result<PathBuf, String> {
    let argv = vec!["git".into(), "rev-parse".into(), "--git-common-dir".into()];
    let raw = crate::module::runtime::run_bounded_argv_for(
        &cwd.to_path_buf(),
        &argv,
        cancelled,
        timeout,
    )?;
    let path = PathBuf::from(raw.trim());
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn validate_bounded(
    repo: &Path,
    path: &Path,
    branch: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    if !path.is_dir() {
        return Err(format!(
            "worktree provider returned a missing directory: {}",
            path.display()
        ));
    }
    let run = |cwd: &Path, args: &[&str]| {
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        crate::module::runtime::run_bounded_argv(&cwd.to_path_buf(), &argv, cancelled)
    };
    if run(path, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
        return Err(format!(
            "worktree provider returned a path that is not a Git worktree: {}",
            path.display()
        ));
    }
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "could not resolve returned worktree path {}: {error}",
            path.display()
        )
    })?;
    let raw = run(repo, &["worktree", "list", "--porcelain", "-z"])
        .or_else(|_| run(repo, &["worktree", "list", "--porcelain"]))?;
    let registered = raw
        .split(['\0', '\n'])
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|candidate| {
            std::fs::canonicalize(candidate)
                .map(|candidate| crate::platform::same_path(&candidate, &canonical_path))
                .unwrap_or(false)
        });
    if !registered {
        return Err(format!(
            "worktree provider returned a path not registered as a Git worktree: {}",
            path.display()
        ));
    }
    let common_dir = |cwd: &Path| -> Result<PathBuf, String> {
        let raw = run(cwd, &["rev-parse", "--git-common-dir"])?;
        let path = PathBuf::from(raw.trim());
        let absolute = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
    };
    let source_common = common_dir(repo)?;
    let worktree_common = common_dir(path)?;
    if !crate::platform::same_path(&source_common, &worktree_common) {
        return Err(format!(
            "worktree provider returned a checkout from a different repository: {}",
            path.display()
        ));
    }
    let actual = run(path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if actual.trim() != branch {
        return Err(format!(
            "worktree provider returned branch {:?}, expected {branch:?}",
            actual.trim()
        ));
    }
    Ok(())
}

fn validate(repo: &Path, path: &Path, branch: &str) -> Result<(), String> {
    if !path.is_dir() {
        return Err(format!(
            "worktree provider returned a missing directory: {}",
            path.display()
        ));
    }
    if !crate::git::local::is_repo(path) {
        return Err(format!(
            "worktree provider returned a path that is not a Git worktree: {}",
            path.display()
        ));
    }
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        format!(
            "could not resolve returned worktree path {}: {error}",
            path.display()
        )
    })?;
    let registered = crate::git::local::worktrees(repo)?
        .into_iter()
        .filter(|worktree| !worktree.bare)
        .any(|worktree| {
            std::fs::canonicalize(&worktree.path)
                .map(|candidate| crate::platform::same_path(&candidate, &canonical_path))
                .unwrap_or(false)
        });
    if !registered {
        return Err(format!(
            "worktree provider returned a path not registered as a Git worktree: {}",
            path.display()
        ));
    }
    let source_common = crate::git::local::common_dir(repo)
        .ok_or_else(|| "could not resolve the source repository common dir".to_string())?;
    let worktree_common = crate::git::local::common_dir(path).ok_or_else(|| {
        format!(
            "could not resolve the returned worktree common dir: {}",
            path.display()
        )
    })?;
    if !crate::platform::same_path(&source_common, &worktree_common) {
        return Err(format!(
            "worktree provider returned a checkout from a different repository: {}",
            path.display()
        ));
    }
    let actual = crate::git::local::current_branch(path)?;
    if actual != branch {
        return Err(format!(
            "worktree provider returned branch {actual:?}, expected {branch:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_a_provider_id() {
        assert!(validate_config(
            &WorktreeConfig {
                provider: "  ".into()
            },
            None,
        )
        .is_err());
        assert!(validate_config(&WorktreeConfig::default(), None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_post_validation_cleans_created_worktree_and_branch() {
        let base = std::env::temp_dir().join(format!(
            "luvus-worktree-cancel-cleanup-{}-{}",
            std::process::id(),
            crate::automation::unix_now()
        ));
        let repo = base.join("repo");
        let path = base.join("topic");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"], &repo);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
            &repo,
        );
        git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "topic",
                path.to_str().unwrap(),
            ],
            &repo,
        );
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let error = validate_bounded(&repo, &path, "topic", &cancelled).unwrap_err();
        assert!(error.contains("cancelled"));
        cleanup_failed_creation(&repo, &path, "topic", true, &[]);
        assert!(!path.exists());
        assert!(!crate::git::local::branch_exists(&repo, "topic"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn failed_validation_never_removes_a_preexisting_worktree() {
        let base = std::env::temp_dir().join(format!(
            "luvus-worktree-existing-safe-{}-{}",
            std::process::id(),
            crate::automation::unix_now()
        ));
        let repo = base.join("repo");
        let path = base.join("existing");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"], &repo);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
            &repo,
        );
        git(
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "existing",
                path.to_str().unwrap(),
            ],
            &repo,
        );
        let existing = vec![path.clone()];
        assert!(worktree_existed(&path, &existing));
        cleanup_failed_creation(&repo, &path, "requested", true, &existing);
        assert!(path.exists());
        assert!(crate::git::local::branch_exists(&repo, "existing"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn parses_provider_json_path_and_rejects_other_output() {
        let path = if cfg!(windows) {
            r#"C:\\repo.wt"#
        } else {
            "/repo.wt"
        };
        let json = format!(r#"{{"path":"{path}"}}"#);
        assert_eq!(
            parse_provider_output(json.as_bytes()).unwrap(),
            PathBuf::from(path.replace("\\\\", "\\"))
        );
        assert!(parse_provider_output(b"human log\n{\"path\":\"/repo.wt\"}").is_err());
        assert!(parse_provider_output(br#"{"path":"relative"}"#).is_err());
    }
}
