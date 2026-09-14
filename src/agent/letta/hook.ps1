# Luvus Letta Code integration. Letta passes one SessionStart JSON payload on
# stdin. The Luvus binary reads it with a strict bound and reports only the
# exact conversation identity to the inherited owner-local server.

if (
    $env:LUVUS_ENV -ne "1" -or
    [string]::IsNullOrWhiteSpace($env:LUVUS_SOCKET_PATH) -or
    [string]::IsNullOrWhiteSpace($env:LUVUS_PANE_ID)
) {
    exit 0
}

$luvus = if ([string]::IsNullOrWhiteSpace($env:LUVUS_BIN_PATH)) { "luvus" } else { $env:LUVUS_BIN_PATH }
try {
    & $luvus integration hook letta *> $null
} catch {
    # Hooks are optional telemetry into the local Luvus server. Never interrupt
    # Letta startup when the server or installed binary is unavailable.
}
exit 0
