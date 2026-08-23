#Requires -Version 5
# luvus installer for Windows — downloads the prebuilt binary from the GitHub
# releases and adds it to your PATH (no admin needed).
#
#   irm https://luvus.dev/install.ps1 | iex
#
# Overrides (set before running):
#   $env:LUVUS_VERSION     = 'v0.1.0'    # a specific tag (default: latest release)
#   $env:LUVUS_INSTALL_DIR = 'C:\tools'  # where to put luvus.exe (default: %LOCALAPPDATA%\luvus)
$ErrorActionPreference = 'Stop'
# Older Windows PowerShell defaults to TLS 1.0/1.1, which GitHub rejects.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'RizRiyz/luvus'
$Bin  = 'luvus'

function Fail($msg) { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

# ── target (the release ships x64 Windows only) ──
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
  Fail "unsupported Windows arch: $env:PROCESSOR_ARCHITECTURE (only x64 prebuilt binaries exist; install with: cargo install --git https://github.com/$Repo)"
}
$target = 'x86_64-pc-windows-msvc'

# ── resolve version ──
if (-not $env:LUVUS_VERSION -and $env:BOHAY_VERSION) { $env:LUVUS_VERSION = $env:BOHAY_VERSION }
if ($env:LUVUS_VERSION) {
  $tag = $env:LUVUS_VERSION
} else {
  try {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" `
      -Headers @{ 'User-Agent' = 'luvus-installer' }
    $tag = $rel.tag_name
  } catch {
    Fail "could not reach the GitHub API ($($_.Exception.Message))"
  }
}
if (-not $tag) { Fail "could not find the latest release (set LUVUS_VERSION to a tag like v0.1.0)" }

$asset = "$Bin-$tag-$target.zip"
$url   = "https://github.com/$Repo/releases/download/$tag/$asset"
Write-Host "Installing $Bin $tag ($target)..."

# ── download + extract ──
$tmp = Join-Path $env:TEMP ("luvus-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $zip = Join-Path $tmp $asset
  try {
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  } catch {
    Fail "download failed: $url"
  }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exe = Join-Path $tmp "$Bin.exe"
  if (-not (Test-Path $exe)) { Fail "archive did not contain $Bin.exe" }

  if (-not $env:LUVUS_INSTALL_DIR -and $env:BOHAY_INSTALL_DIR) { $env:LUVUS_INSTALL_DIR = $env:BOHAY_INSTALL_DIR }
  $dir = if ($env:LUVUS_INSTALL_DIR) { $env:LUVUS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'luvus' }
  New-Item -ItemType Directory -Path $dir -Force | Out-Null
  Copy-Item $exe (Join-Path $dir "$Bin.exe") -Force
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "OK installed to $dir\$Bin.exe" -ForegroundColor Green

# ── add to the user PATH if it's not there yet ──
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';' | Where-Object { $_ }) -notcontains $dir) {
  $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
  [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
  $env:Path = "$env:Path;$dir"
  Write-Host "Added $dir to your user PATH. Open a new terminal, then run: $Bin"
} else {
  Write-Host "Run: $Bin"
}
