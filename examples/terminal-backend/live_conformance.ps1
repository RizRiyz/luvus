param(
    [string]$Luvus = (Join-Path $PSScriptRoot "..\..\target\debug\luvus.exe")
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not (Test-Path -LiteralPath $Luvus -PathType Leaf)) {
    throw "Luvus binary not found at '$Luvus'. Run 'cargo build' first or pass -Luvus <path>."
}
$Binary = (Resolve-Path $Luvus).Path
$State = Join-Path $Root "target\terminal-backend-windows-$PID"
$PreviousHome = $env:LUVUS_HOME
$PreviousSocket = $env:LUVUS_SOCKET_PATH
$PreviousSession = $env:LUVUS_SESSION
$Server = $null
$EventConnection = $null

Add-Type @"
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class LuvusNamedPipeNative
{
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetNamedPipeServerProcessId(
        SafePipeHandle pipe,
        out uint serverProcessId);
}
"@

function Open-PipeConnection {
    param([Parameter(Mandatory)][string]$Address)

    $Prefix = "\\.\pipe\"
    if (-not $Address.StartsWith($Prefix, [System.StringComparison]::Ordinal)) {
        throw "discovery returned a non-local Windows pipe address: $Address"
    }
    $Name = $Address.Substring($Prefix.Length)
    if ([string]::IsNullOrWhiteSpace($Name) -or $Name.Contains("\")) {
        throw "discovery returned an invalid Windows pipe name"
    }
    $Pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
        ".",
        $Name,
        [System.IO.Pipes.PipeDirection]::InOut,
        [System.IO.Pipes.PipeOptions]::Asynchronous
    )
    $Pipe.Connect(5000)
    $Encoding = [System.Text.UTF8Encoding]::new($false)
    $Reader = [System.IO.StreamReader]::new($Pipe, $Encoding, $false, 4096, $true)
    $Writer = [System.IO.StreamWriter]::new($Pipe, $Encoding, 4096, $true)
    $Writer.AutoFlush = $true
    [pscustomobject]@{ Pipe = $Pipe; Reader = $Reader; Writer = $Writer }
}

function Close-PipeConnection {
    param($Connection)

    if ($null -eq $Connection) {
        return
    }
    foreach ($Part in @($Connection.Reader, $Connection.Writer, $Connection.Pipe)) {
        if ($null -ne $Part) {
            try { $Part.Dispose() } catch { }
        }
    }
}

function Read-BoundedLine {
    param(
        [Parameter(Mandatory)]$Reader,
        [int]$TimeoutMs = 5000
    )

    $Task = $Reader.ReadLineAsync()
    if (-not $Task.Wait($TimeoutMs)) {
        throw "named-pipe response timed out"
    }
    $Line = $Task.Result
    if ($null -eq $Line) {
        throw "named-pipe response ended before LF"
    }
    if ([System.Text.Encoding]::UTF8.GetByteCount($Line) + 1 -gt 1MB) {
        throw "named-pipe response exceeded the protocol frame limit"
    }
    $Line
}

function Send-Request {
    param(
        [Parameter(Mandatory)][string]$Address,
        [Parameter(Mandatory)][hashtable]$Request
    )

    $Connection = Open-PipeConnection $Address
    try {
        $Json = $Request | ConvertTo-Json -Compress -Depth 20
        if ([System.Text.Encoding]::UTF8.GetByteCount($Json) + 1 -gt 1MB) {
            throw "named-pipe request exceeded the protocol frame limit"
        }
        $Connection.Writer.Write($Json + "`n")
        $Connection.Writer.Flush()
        $Response = (Read-BoundedLine $Connection.Reader) | ConvertFrom-Json
        if ($Response.id -ne $Request.id) {
            throw "named-pipe response id mismatch"
        }
        $Response
    } finally {
        Close-PipeConnection $Connection
    }
}

function Wait-TerminalEvent {
    param(
        [Parameter(Mandatory)]$Connection,
        [Parameter(Mandatory)][string]$Name,
        [string]$TerminalId = "",
        [int]$TimeoutMs = 30000
    )

    $Deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $Deadline) {
        $RemainingMs = [Math]::Max(1, [int]($Deadline - [DateTime]::UtcNow).TotalMilliseconds)
        $Received = (Read-BoundedLine -Reader $Connection.Reader -TimeoutMs $RemainingMs) | ConvertFrom-Json
        if ($Received.event -eq "terminal.resync_required") {
            throw "terminal event stream overflowed during conformance"
        }
        if ($Received.event -eq $Name -and
            ([string]::IsNullOrEmpty($TerminalId) -or $Received.data.terminal_id -eq $TerminalId)) {
            return $Received
        }
    }
    throw "did not receive expected event $Name"
}

try {
    Remove-Item -LiteralPath $State -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $State | Out-Null
    $env:LUVUS_HOME = $State
    Remove-Item Env:LUVUS_SOCKET_PATH, Env:LUVUS_SESSION -ErrorAction SilentlyContinue

    $Server = Start-Process -FilePath $Binary -ArgumentList @("server") -PassThru -WindowStyle Hidden
    $Session = $null
    for ($Attempt = 0; $Attempt -lt 200; $Attempt++) {
        if ($Server.HasExited) {
            throw "isolated Windows Luvus server exited during startup"
        }
        try {
            $Sessions = (& $Binary session list --json | ConvertFrom-Json).sessions
            $Session = $Sessions | Where-Object { $_.default -and $_.running } | Select-Object -First 1
            if ($null -ne $Session) {
                break
            }
        } catch {
        }
        Start-Sleep -Milliseconds 25
    }
    if ($null -eq $Session) {
        throw "isolated Windows Luvus server did not become discoverable"
    }
    if ($Session.endpoint.transport -ne "windows_named_pipe") {
        throw "Windows discovery returned $($Session.endpoint.transport)"
    }
    $Address = [string]$Session.endpoint.address
    $IdentityProbe = Open-PipeConnection $Address
    try {
        [uint32]$PipeServerPid = 0
        if (-not [LuvusNamedPipeNative]::GetNamedPipeServerProcessId(
            $IdentityProbe.Pipe.SafePipeHandle,
            [ref]$PipeServerPid
        )) {
            throw "could not identify the named-pipe server process"
        }
        if ($PipeServerPid -ne $Server.Id) {
            throw "discovery connected to an unexpected named-pipe server process"
        }
    } finally {
        Close-PipeConnection $IdentityProbe
    }

    $Capability = Send-Request $Address @{
        id = "cap"
        method = "terminal.backend.capabilities"
        params = @{ protocol = @{ name = "luvus-terminal-backend"; major = 1; minor = 0 } }
    }
    if ($Capability.result.protocol.major -ne 1 -or $Capability.result.protocol.minor -ne 0) {
        throw "terminal backend did not negotiate protocol 1.0"
    }
    $Incompatible = Send-Request $Address @{
        id = "incompatible"
        method = "terminal.backend.capabilities"
        params = @{ protocol = @{ name = "luvus-terminal-backend"; major = 1; minor = 1 } }
    }
    if ($Incompatible.error.code -ne "incompatible_protocol") {
        throw "terminal backend accepted an incompatible protocol"
    }

    $EventConnection = Open-PipeConnection $Address
    $EventConnection.Writer.Write((@{
        id = "events"
        method = "terminal.backend.events.subscribe"
        params = @{}
    } | ConvertTo-Json -Compress) + "`n")
    $EventConnection.Writer.Flush()
    $Subscribed = (Read-BoundedLine $EventConnection.Reader) | ConvertFrom-Json
    if ($Subscribed.result.type -ne "subscription_started") {
        throw "terminal event subscription did not start"
    }

    $Created = Send-Request $Address @{
        id = "create"
        method = "terminal.backend.create"
        params = @{
            cwd = $Root
            command = @("cmd.exe", "/Q", "/K")
            label = "windows-conformance"
            placement = @{ kind = "workspace" }
            focus = $false
        }
    }
    if ($Created.result.dispatch -ne "executed") {
        throw "Windows terminal creation was not executed"
    }
    $TerminalId = [string]$Created.result.terminal_id
    $PaneId = [string]$Created.result.pane_id
    $Generation = [string]$Created.result.server_generation
    $StartMarker = [string]$Created.result.root_process.start_marker
    if (-not $StartMarker.StartsWith("windows:")) {
        throw "Windows terminal did not expose a creation-time start marker"
    }
    $CreatedEvent = Wait-TerminalEvent $EventConnection "terminal.created" $TerminalId
    if ($CreatedEvent.data.pane_id -ne $PaneId) {
        throw "created event targeted the wrong pane"
    }

    $Capture = Send-Request $Address @{
        id = "capture"
        method = "terminal.backend.capture"
        params = @{
            server_generation = $Generation
            terminal_id = $TerminalId
            pane_id = $PaneId
            expected_root = @{ pid = $Created.result.root_process.pid; start_marker = $StartMarker }
            mode = "recent_unwrapped"
            lines = 50
            ansi = $false
        }
    }
    $Revision = [uint64]$Capture.result.content_revision
    $Submitted = Send-Request $Address @{
        id = "submit"
        method = "terminal.backend.submit_text"
        params = @{
            server_generation = $Generation
            terminal_id = $TerminalId
            pane_id = $PaneId
            expected_root = @{ pid = $Created.result.root_process.pid; start_marker = $StartMarker }
            text = "echo LUVUS_WINDOWS_CONFORMANCE"
        }
    }
    if ($Submitted.result.dispatch -ne "queued") {
        throw "Windows terminal input was not queued"
    }
    $Output = Send-Request $Address @{
        id = "wait-output"
        method = "terminal.backend.wait_output"
        params = @{
            server_generation = $Generation
            terminal_id = $TerminalId
            pane_id = $PaneId
            expected_root = @{ pid = $Created.result.root_process.pid; start_marker = $StartMarker }
            after_revision = $Revision
            match = "LUVUS_WINDOWS_CONFORMANCE"
            timeout_ms = 10000
        }
    }
    if ($Output.result.type -ne "terminal_backend_output") {
        throw "Windows output wait did not observe submitted text"
    }

    $Processes = $null
    for ($Attempt = 0; $Attempt -lt 80; $Attempt++) {
        $Processes = Send-Request $Address @{
            id = "processes"
            method = "terminal.backend.processes"
            params = @{
                server_generation = $Generation
                terminal_id = $TerminalId
                pane_id = $PaneId
            }
        }
        if ($Processes.result.scan -eq "observed") {
            break
        }
        Start-Sleep -Milliseconds 50
    }
    if ($Processes.result.scan -ne "observed" -or
        -not ($Processes.result.executables -contains "cmd")) {
        throw "Windows process discovery did not observe cmd.exe"
    }

    $Closed = Send-Request $Address @{
        id = "close"
        method = "terminal.backend.close"
        params = @{
            server_generation = $Generation
            terminal_id = $TerminalId
            pane_id = $PaneId
            expected_root = @{ pid = $Created.result.root_process.pid; start_marker = $StartMarker }
        }
    }
    if ($Closed.result.dispatch -ne "executed") {
        throw "Windows terminal close was not executed"
    }
    Wait-TerminalEvent $EventConnection "terminal.closed" $TerminalId | Out-Null

    Write-Host "terminal-backend live conformance passed on Windows named pipes and ConPTY"
} finally {
    Close-PipeConnection $EventConnection
    try {
        if (Test-Path -LiteralPath $Binary) {
            & $Binary server stop | Out-Null
        }
    } catch {
    }
    if ($null -ne $Server -and -not $Server.HasExited) {
        Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $State -Recurse -Force -ErrorAction SilentlyContinue
    if ($null -eq $PreviousHome) { Remove-Item Env:LUVUS_HOME -ErrorAction SilentlyContinue } else { $env:LUVUS_HOME = $PreviousHome }
    if ($null -eq $PreviousSocket) { Remove-Item Env:LUVUS_SOCKET_PATH -ErrorAction SilentlyContinue } else { $env:LUVUS_SOCKET_PATH = $PreviousSocket }
    if ($null -eq $PreviousSession) { Remove-Item Env:LUVUS_SESSION -ErrorAction SilentlyContinue } else { $env:LUVUS_SESSION = $PreviousSession }
}
