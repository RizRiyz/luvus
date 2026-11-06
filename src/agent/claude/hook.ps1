# Luvus Claude Code integration for Windows. Claude Code passes one hook JSON
# payload on stdin, and the Luvus binary reports the session or lifecycle event
# to the pane running it (`luvus integration hook claude`), so the hook needs
# neither python3 nor a POSIX shell.

if ($env:LUVUS_ENV -ne "1" -or [string]::IsNullOrWhiteSpace($env:LUVUS_SOCKET_PATH)) {
    exit 0
}
# Devin imports Claude-format hooks (global and project `.claude/settings.json`)
# and marks every hook process it spawns. Never report its sessions as ours.
if (-not [string]::IsNullOrEmpty($env:DEVIN_PROJECT_DIR)) {
    exit 0
}

$luvus = $env:LUVUS_BIN_PATH
if ([string]::IsNullOrWhiteSpace($luvus) -or -not (Test-Path -LiteralPath $luvus -PathType Leaf)) {
    $luvus = "luvus"
}
try {
    & $luvus integration hook claude *> $null
} catch {
    # Hooks are optional telemetry into the local Luvus server. Never interrupt
    # Claude Code when the server or installed binary is unavailable.
}
exit 0
