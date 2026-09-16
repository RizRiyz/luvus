//! Configurable worktree creation backends.
//!
//! Creation is the only provider-controlled operation. Removal and ORCH merge
//! continue to use Git directly. Every backend returns a candidate path, which
//! is verified against Git before callers may open a workspace there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::config::WorktreeConfig;

const FORBIDDEN_EXACT: &[&str] = &[
    "-C",
    "--config",
    "--config-set",
    "--format",
    "-x",
    "--execute",
    "--no-cd",
    "--cd",
    "-c",
    "--create",
    "-b",
    "--base",
    "--branches",
    "--remotes",
    "--prs",
    "--clobber",
    "-y",
    "--yes",
    "--",
];

#[derive(Deserialize)]
struct WorktrunkOutput {
    path: PathBuf,
}

/// Create `branch` using the configured backend and verify the returned path.
pub fn create(
    config: &WorktreeConfig,
    repo: &Path,
    git_path: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    let provider = config.provider.trim().to_ascii_lowercase();
    if provider == "worktrunk" && branch.starts_with('-') {
        return Err("branch names beginning with '-' are not allowed with Worktrunk".to_string());
    }
    let path = match provider.as_str() {
        "git" => {
            crate::git::local::worktree_add(repo, git_path, branch)?;
            git_path.to_path_buf()
        }
        "worktrunk" => create_with_worktrunk(config, repo, branch)?,
        provider => return Err(format!("unknown worktree provider: {provider}")),
    };
    validate(repo, &path, branch)?;
    Ok(path)
}

pub(crate) fn validate_config(config: &WorktreeConfig) -> Result<(), String> {
    if config.provider.trim().eq_ignore_ascii_case("worktrunk") {
        if config.executable.trim().is_empty() {
            return Err("worktree.executable must not be empty".to_string());
        }
        validate_extra_args(&config.args)?;
    }
    Ok(())
}

fn create_with_worktrunk(
    config: &WorktreeConfig,
    repo: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    let executable = config.executable.trim();
    if executable.is_empty() {
        return Err("worktree.executable must not be empty".to_string());
    }
    let args = worktrunk_args(repo, branch, &config.args)?;

    let output = crate::platform::no_window(Command::new(executable).args(&args))
        .output()
        .map_err(|error| format!("failed to run {executable}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("{executable} exited with {}", output.status)
        } else {
            stderr
        });
    }
    parse_worktrunk_output(&output.stdout)
}

fn worktrunk_args(repo: &Path, branch: &str, extra: &[String]) -> Result<Vec<OsString>, String> {
    validate_extra_args(extra)?;
    let mut args = vec![
        OsString::from("-C"),
        repo.as_os_str().to_owned(),
        OsString::from("switch"),
    ];
    args.extend(extra.iter().map(OsString::from));
    if !crate::git::local::branch_exists(repo, branch) {
        args.push(OsString::from("--create"));
    }
    args.extend([
        OsString::from(branch),
        OsString::from("--no-cd"),
        OsString::from("--format"),
        OsString::from("json"),
    ]);
    Ok(args)
}

fn validate_extra_args(args: &[String]) -> Result<(), String> {
    for arg in args {
        if arg.is_empty() {
            return Err("worktree.args must not contain an empty argument".to_string());
        }
        let option = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        let short = option
            .strip_prefix('-')
            .filter(|value| !value.starts_with('-'));
        let protected_short = short.is_some_and(|value| {
            value
                .chars()
                .any(|flag| matches!(flag, 'C' | 'x' | 'c' | 'b' | 'y'))
        });
        if FORBIDDEN_EXACT.contains(&option) || option.starts_with("--config-") || protected_short {
            return Err(format!(
                "worktree.args contains protected Worktrunk argument: {arg}"
            ));
        }
        if !arg.starts_with('-') {
            return Err(format!(
                "worktree.args may contain only option tokens, not a branch or command: {arg}"
            ));
        }
    }
    Ok(())
}

fn parse_worktrunk_output(stdout: &[u8]) -> Result<PathBuf, String> {
    let output: WorktrunkOutput = serde_json::from_slice(stdout)
        .map_err(|error| format!("Worktrunk returned invalid JSON: {error}"))?;
    if !output.path.is_absolute() {
        return Err("Worktrunk returned a non-absolute worktree path".to_string());
    }
    Ok(output.path)
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
    fn rejects_contract_conflicts_and_positional_tokens() {
        for arg in [
            "-C",
            "-C/tmp/x",
            "--config=x",
            "--config-set",
            "--format=json",
            "-x",
            "-xcode",
            "--execute=code",
            "--no-cd",
            "--cd",
            "-c",
            "--create",
            "--base=main",
            "-b",
            "--branches",
            "--remotes",
            "--prs",
            "--clobber",
            "--yes",
            "-y",
            "-vc",
            "-vy",
            "-vb",
            "--",
            "topic",
        ] {
            let error = validate_extra_args(&[arg.to_string()]).unwrap_err();
            assert!(error.contains(arg), "{arg}: {error}");
        }
    }

    #[test]
    fn accepts_non_conflicting_option_tokens() {
        assert!(validate_extra_args(&["--no-hooks".into(), "-vv".into()]).is_ok());
    }

    #[test]
    fn rejects_option_shaped_branch_before_constructing_argv() {
        let config = WorktreeConfig {
            provider: "worktrunk".into(),
            executable: "must-not-run".into(),
            args: Vec::new(),
        };
        let error = create(
            &config,
            Path::new("/unused/repo"),
            Path::new("/unused/worktree"),
            "--execute=sh",
        )
        .unwrap_err();
        assert!(error.contains("not allowed with Worktrunk"));
    }

    #[test]
    fn config_validation_trims_provider_before_protecting_worktrunk() {
        let config = WorktreeConfig {
            provider: " worktrunk ".into(),
            executable: "wt".into(),
            args: vec!["--yes".into()],
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn parses_json_path_and_rejects_other_output() {
        let path = if cfg!(windows) {
            r#"C:\\repo.wt"#
        } else {
            "/repo.wt"
        };
        let json = format!(
            r#"{{"action":"created","branch":"topic","path":"{path}","created_branch":true,"base_branch":"main"}}"#
        );
        assert_eq!(
            parse_worktrunk_output(json.as_bytes()).unwrap(),
            PathBuf::from(path.replace("\\\\", "\\"))
        );
        assert!(parse_worktrunk_output(b"human log\n{\"path\":\"/repo.wt\"}").is_err());
        assert!(parse_worktrunk_output(br#"{"path":"relative"}"#).is_err());
    }

    #[cfg(unix)]
    fn test_repo(tag: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("luvus-provider-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        (base, repo)
    }

    #[cfg(unix)]
    fn write_fake_worktrunk(base: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let executable = base.join("fake-wt");
        std::fs::write(&executable, body).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        executable
    }

    #[cfg(unix)]
    #[test]
    fn worktrunk_creates_new_branch_with_fixed_argv_and_parses_stdout_only() {
        let (base, repo) = test_repo("new");
        let path = base.join("topic-wt");
        let fake = write_fake_worktrunk(
            &base,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > "$0.args"
repo=$2
shift 3
create=
if [ "$1" = --create ]; then create=1; shift; fi
branch=$1
if [ -n "$create" ]; then
  git -C "$repo" worktree add -q -b "$branch" '{}'
else
  git -C "$repo" worktree add -q '{}' "$branch"
fi
printf '%s\n' 'human hook log' >&2
printf '%s\n' '{{"action":"created","branch":"topic","path":"{}","created_branch":true,"base_branch":"main"}}'
"#,
                path.display(),
                path.display(),
                path.display()
            ),
        );
        let config = WorktreeConfig {
            provider: "worktrunk".into(),
            executable: fake.display().to_string(),
            args: Vec::new(),
        };

        assert_eq!(
            create(&config, &repo, &base.join("unused"), "topic").unwrap(),
            path
        );
        let args = std::fs::read_to_string(fake.with_extension("args")).unwrap();
        let expected = format!(
            "-C\n{}\nswitch\n--create\ntopic\n--no-cd\n--format\njson\n",
            repo.display()
        );
        assert_eq!(args, expected);
        assert!(!args.contains("--yes"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn existing_local_branch_omits_create_from_worktrunk_argv() {
        let (base, repo) = test_repo("existing");
        let args = worktrunk_args(&repo, "main", &["--no-hooks".into()]).unwrap();
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-C",
                repo.to_str().unwrap(),
                "switch",
                "--no-hooks",
                "main",
                "--no-cd",
                "--format",
                "json",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--create"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn worktrunk_failure_preserves_stderr() {
        let (base, repo) = test_repo("stderr");
        let fake = write_fake_worktrunk(
            &base,
            "#!/bin/sh\nprintf '%s\\n' 'Cannot prompt for approval in non-interactive environment' >&2\nexit 2\n",
        );
        let config = WorktreeConfig {
            provider: "worktrunk".into(),
            executable: fake.display().to_string(),
            args: Vec::new(),
        };
        let error = create(&config, &repo, &base.join("unused"), "topic").unwrap_err();
        assert_eq!(
            error,
            "Cannot prompt for approval in non-interactive environment"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn validation_rejects_wrong_branch_and_unregistered_directory() {
        let (base, repo) = test_repo("validate");
        let worktree = base.join("main-wt");
        let output = Command::new("git")
            .args(["worktree", "add", "-q", "-b", "actual"])
            .arg(&worktree)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(validate(&repo, &worktree, "expected")
            .unwrap_err()
            .contains("actual"));
        let ordinary = base.join("ordinary");
        std::fs::create_dir_all(&ordinary).unwrap();
        assert!(validate(&repo, &ordinary, "actual").is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
