//! Safe native invocation across OpenSSH's POSIX, cmd.exe and PowerShell shells.

use std::process::Command;

pub(crate) fn append(command: &mut Command, binary: &str, args: &[&str]) -> anyhow::Result<()> {
    super::catalog::validate_remote_binary(binary)?;
    if args.iter().any(|arg| {
        arg.is_empty()
            || !arg
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
    }) {
        anyhow::bail!("invalid remote machine command argument");
    }
    if binary.contains(' ') {
        // Do not send native stdout through a PowerShell pipeline: that would
        // decode/re-encode the binary client protocol. Start the child with
        // raw byte streams instead, with no shell interpretation.
        let script = windows_script(binary, args);
        let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        command
            .args([
                "powershell.exe",
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
            ])
            .arg(crate::base64_encode(&bytes));
    } else {
        command.arg(binary).args(args);
    }
    Ok(())
}

fn windows_script(binary: &str, args: &[&str]) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$info = New-Object System.Diagnostics.ProcessStartInfo
$info.FileName = '{binary}'
$info.Arguments = '{}'
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardInput = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$child = [System.Diagnostics.Process]::Start($info)
$inputCopy = [Console]::OpenStandardInput().CopyToAsync($child.StandardInput.BaseStream)
$outputCopy = $child.StandardOutput.BaseStream.CopyToAsync([Console]::OpenStandardOutput())
$errorCopy = $child.StandardError.BaseStream.CopyToAsync([Console]::OpenStandardError())
$inputClosed = $false
while (-not $child.WaitForExit(50)) {{
 if (-not $inputClosed -and $inputCopy.IsCompleted) {{
  $child.StandardInput.Close()
  $inputClosed = $true
 }}
}}
$outputCopy.GetAwaiter().GetResult()
$errorCopy.GetAwaiter().GetResult()
$code = $child.ExitCode
$child.Dispose()
exit $code
"#,
        args.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_binary_stream_child() {
        if std::env::var_os("LUVUS_TEST_MACHINE_STREAM").is_none() {
            return;
        }
        use std::io::{Read, Write};
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        std::io::stdout().write_all(&bytes).unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr().write_all(b"machine-stderr").unwrap();
        std::process::exit(37);
    }

    #[cfg(windows)]
    #[test]
    fn windows_space_path_preserves_binary_stdio_under_both_shells() {
        let _env = crate::persist::test_env("machine-space-path");
        let directory = crate::persist::config_dir().join("Alice Smith");
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("luvus.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        let script = windows_script(
            binary.to_str().unwrap(),
            &[
                "--exact",
                "machine::command::tests::windows_binary_stream_child",
                "--nocapture",
            ],
        );
        let bytes: Vec<_> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = crate::base64_encode(&bytes);
        let remote =
            format!("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}");
        // Leave room for the Rust test harness within the probe's 64 KiB cap.
        let payload: Vec<u8> = (0..=255).cycle().take(8192).collect();
        for shell in ["cmd.exe", "powershell.exe"] {
            let mut command = Command::new(shell);
            if shell == "cmd.exe" {
                command.args(["/d", "/s", "/c"]);
            } else {
                command.args(["-NoProfile", "-NonInteractive", "-Command"]);
            }
            command.arg(&remote).env("LUVUS_TEST_MACHINE_STREAM", "1");
            crate::platform::no_window(&mut command);
            let output = super::super::ssh::run_bounded_with_input(
                command,
                std::time::Duration::from_secs(30),
                Some(&payload),
            )
            .unwrap_or_else(|error| panic!("{shell} binary stream failed: {error:#}"));
            // PowerShell -Command maps a failing nested native process to 1;
            // cmd.exe retains 37. Both must preserve failure, not report success.
            assert!(
                !output.status.success(),
                "{shell} lost child failure status"
            );
            assert!(
                output.stdout.ends_with(&payload),
                "{shell} changed protocol bytes"
            );
            assert_eq!(output.stderr, b"machine-stderr");
        }
    }

    #[test]
    fn windows_space_paths_use_encoded_native_launch_without_text_pipes() {
        let binary = r"C:\Users\Alice Smith\AppData\Local\luvus\luvus.exe";
        let mut command = Command::new("ssh");
        append(
            &mut command,
            binary,
            &["--session", "review", "remote-client-bridge"],
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[0], "powershell.exe");
        assert_eq!(args[4], "-EncodedCommand");
        let script = windows_script(binary, &["--session", "review", "remote-client-bridge"]);
        assert!(script.contains("$info.FileName = 'C:\\Users\\Alice Smith"));
        assert!(script.contains("$info.UseShellExecute = $false"));
        assert!(script.contains("$info.CreateNoWindow = $true"));
        assert!(script.contains("$info.RedirectStandardInput = $true"));
        assert!(script.contains("$child.StandardInput.Close()"));
        assert!(script.contains("StandardOutput.BaseStream.CopyToAsync"));
        assert!(script.contains("StandardError.BaseStream.CopyToAsync"));
        assert!(!script.contains('|'));
        assert!(append(&mut command, binary, &["review;whoami"]).is_err());
        assert!(append(&mut command, r"C:\Users\Alice' Smith\luvus.exe", &[]).is_err());
    }
}
