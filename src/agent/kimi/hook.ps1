# Luvus Kimi Code integration for Windows. Kimi Code runs each hook command
# through cmd.exe and passes one hook JSON payload on stdin; the Luvus binary
# reports the session or lifecycle event to the pane running it
# (`luvus integration hook kimi`), so the hook needs neither python3 nor a
# POSIX shell.

if ($env:LUVUS_ENV -ne "1" -or [string]::IsNullOrWhiteSpace($env:LUVUS_SOCKET_PATH)) {
    exit 0
}

$luvus = $env:LUVUS_BIN_PATH
if ([string]::IsNullOrWhiteSpace($luvus) -or -not (Test-Path -LiteralPath $luvus -PathType Leaf)) {
    $luvus = "luvus"
}
try {
    & $luvus integration hook kimi *> $null
} catch {
    # Hooks are optional telemetry into the local Luvus server. Never interrupt
    # Kimi Code when the server or installed binary is unavailable.
}
exit 0
