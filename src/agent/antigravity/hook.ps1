# Luvus Antigravity CLI integration. Antigravity passes the hook JSON through
# stdin; the Luvus binary parses it with a bounded reader and returns JSON.

if (
    $env:LUVUS_ENV -ne "1" -or
    [string]::IsNullOrWhiteSpace($env:LUVUS_SOCKET_PATH) -or
    [string]::IsNullOrWhiteSpace($env:LUVUS_PANE_ID)
) {
    Write-Output "{}"
    exit 0
}

$luvus = if ([string]::IsNullOrWhiteSpace($env:LUVUS_BIN_PATH)) { "luvus" } else { $env:LUVUS_BIN_PATH }
try {
    & $luvus integration hook antigravity 2>$null
    if ($LASTEXITCODE -ne 0) { Write-Output "{}" }
} catch {
    Write-Output "{}"
}
