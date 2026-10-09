<#
Local Reticulum daemon (rnsd) for Coyote mesh (Windows, PowerShell 5.1+ and 7)

Installs rnsd, writes %USERPROFILE%\.reticulum\config once, and registers a
Scheduled Task that starts the daemon at logon; every Coyote session on this
host then dials it at 127.0.0.1:4242. On Linux/macOS use scripts/mesh-relay.sh.

Examples:
  pwsh -File scripts\mesh-relay.ps1
  pwsh -File scripts\mesh-relay.ps1 -Relay relay.example.org:4242
  $env:BIN_DIR = "$env:USERPROFILE\bin"; pwsh -File scripts\mesh-relay.ps1 -NoService

Parameters:
  -Relay <host:port>  Also dial your team's relay (TCPClientInterface).
  -Version <X>        rns version to install (default: see -Help).
  -BinDir <path>      Where rnsd.cmd goes (default: %LOCALAPPDATA%\coyote\bin). Or set BIN_DIR.
  -NoService          Skip the Scheduled Task.
  -DryRun             Print the plan and write nothing.
  -AllowRoot          Permit running as Administrator.

Exit codes: 0 done or nothing to do; 1 usage; 2 unsupported OS or no way to
install rnsd; 3 task registered but 127.0.0.1:4242 did not come up in 30 s.
#>

[CmdletBinding()]
param(
  [string]$Relay = '',
  [string]$Version = '',
  [string]$BinDir = $env:BIN_DIR,
  [switch]$NoService,
  [switch]$DryRun,
  [switch]$AllowRoot,
  [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Must track `ARG RNS_VERSION` in deployment/propagation-node/Dockerfile (the interop-verified version).
$DefaultRnsSpec = 'rns==1.5.2'

$TaskName = 'Coyote rnsd'
$ListenPort = 4242
$ReadyTimeoutSeconds = 30

function Write-Usage {
  Write-Output 'coyote mesh relay setup (Windows): install rnsd, write its config, run it as a Scheduled Task'
  Write-Output ''
  Write-Output 'Options:'
  Write-Output '  -Relay <host:port>      Also dial your team''s relay (TCPClientInterface)'
  Write-Output "  -Version <X>            rns version to install (default: $($DefaultRnsSpec.Substring(5)))"
  Write-Output '  -BinDir <path>          Where rnsd.cmd goes (default: %LOCALAPPDATA%\coyote\bin)'
  Write-Output '  -NoService              Skip the Scheduled Task'
  Write-Output '  -DryRun                 Print the plan and write nothing'
  Write-Output '  -AllowRoot              Permit running as Administrator'
  Write-Output '  -Help                   Show help'
}

function Write-Info {
  param([string]$Message)
  Write-Output "[coyote-mesh] $Message"
}

function Write-Failure {
  param([string]$Message)
  [Console]::Error.WriteLine("[coyote-mesh] Error: $Message")
}

# Runs a native command with $ErrorActionPreference relaxed so its stderr and
# exit code come back as data instead of terminating errors. Stdout goes to the
# host, not the output stream, so the only return value is the exit code.
function Invoke-Native {
  param([string]$File, [string[]]$Arguments = @(), [switch]$Quiet)
  $ErrorActionPreference = 'Continue'
  try {
    if ($Quiet) { & $File @Arguments *> $null } else { & $File @Arguments | Out-Host }
    return $LASTEXITCODE
  } catch {
    return 127
  }
}

function Test-Rnsd {
  param([string]$Path)
  return (Test-Path -LiteralPath $Path -PathType Leaf) -and ((Invoke-Native -File $Path -Arguments @('--version') -Quiet) -eq 0)
}

function Test-PythonOk {
  param([string]$File, [string[]]$Prefix)
  $probe = @($Prefix) + @('-c', 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)')
  return (Invoke-Native -File $File -Arguments $probe -Quiet) -eq 0
}

function Get-InterfaceText {
  $text = @'
[interfaces]

  # Link-local discovery of other hosts on the same LAN.
  [[Coyote Local]]
    type = AutoInterface
    enabled = Yes

  # The loopback hop every Coyote session on this host dials.
  [[Coyote Sessions]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = 4242

    # Reticulum holds announces from unknown peers by default, which would delay a
    # brand-new Coyote identity's first announce. Off here, as on the repo's test relay.
    ingress_control = No
'@
  if ($script:RelayHost) {
    $text += @"


  # Your team's relay (a propagation node or another rnsd reachable over the network).
  [[Team Relay]]
    type = TCPClientInterface
    enabled = Yes
    target_host = $script:RelayHost
    target_port = $script:RelayPort
"@
  }
  return $text
}

function Get-SettingsText {
  return @'
[reticulum]

# Relay announces and paths between the Coyote sessions that dial in. A transport
# node also forwards traffic for any other Reticulum peer it hears on its interfaces.
enable_transport = True

# Shared instance left at the default so rnstatus, rnpath, Sideband and NomadNet on
# this host attach to this daemon instead of binding 4242 / AutoInterface again.

[logging]

# rnsd applies its -v flags only when this section exists (Reticulum.py:459-467).
# 4 = LOG_INFO, the value RNS's own default config ships.
loglevel = 4

'@
}

function Get-ConfigText {
  $head = @'
# Reticulum configuration written by scripts/mesh-relay.ps1 for Coyote mesh.
# This daemon owns the interfaces; every Coyote session on this host dials it
# with mesh.interfaces: [{type: private, host: 127.0.0.1, port: 4242}].
# Interface options: https://markqvist.github.io/Reticulum/manual/interfaces.html

'@
  return $head + "`n" + (Get-SettingsText) + "`n" + (Get-InterfaceText) + "`n"
}

function Write-FirewallWarning {
  Write-Info 'Firewall: AutoInterface listens for LAN peers, and with enable_transport this rnsd forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured). Windows will ask whether python/rnsd may accept incoming connections; allow it or discovery of LAN hosts will not work.'
}

# Writes to a temp file beside the target and moves it into place, so a crash never leaves a half-written file.
function Write-TextFile {
  param([string]$Path, [string]$Text, [Text.Encoding]$Encoding = (New-Object System.Text.UTF8Encoding($false)))
  $dir = Split-Path -Parent $Path
  $tmp = Join-Path $dir ('.' + (Split-Path -Leaf $Path) + '.' + [IO.Path]::GetRandomFileName())
  [IO.File]::WriteAllText($tmp, $Text, $Encoding)
  Move-Item -LiteralPath $tmp -Destination $Path -Force
}

# cmd.exe reads batch files in the OEM code page, so a shim path with non-ASCII characters must be written in it.
# Code page 65001 (the "Use Unicode UTF-8" system setting) must be BOM-less: cmd.exe does not skip a BOM and would
# try to run the first line as a command.
function Get-OemEncoding {
  if ('System.Text.CodePagesEncodingProvider' -as [type]) {
    [Text.Encoding]::RegisterProvider([Text.CodePagesEncodingProvider]::Instance)
  }
  $codePage = [Globalization.CultureInfo]::CurrentCulture.TextInfo.OEMCodePage
  if ($codePage -eq 65001) { return New-Object System.Text.UTF8Encoding($false) }
  return [Text.Encoding]::GetEncoding($codePage)
}

function Write-Config {
  if (Test-Path -LiteralPath $script:RnsConfig -PathType Leaf) {
    Write-Info "$script:RnsConfig already exists, not touched. Make sure it carries these settings and stanzas:"
    Write-Output ''
    Write-Output (Get-SettingsText)
    Write-Output (Get-InterfaceText)
    Write-Output ''
    if (-not (Select-String -LiteralPath $script:RnsConfig -Pattern '^\s*enable_transport\s*=\s*(true|yes)' -Quiet)) {
      Write-Info "WARNING: $script:RnsConfig does not set enable_transport = True; without it this rnsd will not relay between Coyote sessions."
    }
    if (Select-String -LiteralPath $script:RnsConfig -Pattern 'AutoInterface' -Quiet) { Write-FirewallWarning }
    return
  }
  if ($DryRun) {
    Write-Info "Would write ${script:RnsConfig}:"
    Write-Output ''
    Write-Output (Get-ConfigText)
    Write-FirewallWarning
    return
  }
  New-Item -ItemType Directory -Force -Path $script:RnsDir | Out-Null
  Write-TextFile -Path $script:RnsConfig -Text (Get-ConfigText)
  Write-Info "Wrote $script:RnsConfig"
  Write-FirewallWarning
}

function Resolve-InstallRung {
  $script:InstallRung = ''
  $script:ExistingRnsd = ''
  $script:PythonFile = ''
  $script:PythonPrefix = @()

  if (Test-Rnsd -Path $script:RnsdCmd) {
    $script:InstallRung = 'present'
    $script:ExistingRnsd = $script:RnsdCmd
    return
  }
  $onPath = Get-Command rnsd -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($onPath -and (Test-Rnsd -Path $onPath.Source)) {
    $script:InstallRung = 'present'
    $script:ExistingRnsd = $onPath.Source
    return
  }
  if (Get-Command uv -CommandType Application -ErrorAction SilentlyContinue) {
    $script:InstallRung = 'uv'
    return
  }
  if (Get-Command pipx -CommandType Application -ErrorAction SilentlyContinue) {
    $script:InstallRung = 'pipx'
    return
  }
  if ((Get-Command py -CommandType Application -ErrorAction SilentlyContinue) -and (Test-PythonOk -File 'py' -Prefix @('-3'))) {
    $script:InstallRung = 'venv'
    $script:PythonFile = 'py'
    $script:PythonPrefix = @('-3')
    return
  }
  if ((Get-Command python -CommandType Application -ErrorAction SilentlyContinue) -and (Test-PythonOk -File 'python' -Prefix @())) {
    $script:InstallRung = 'venv'
    $script:PythonFile = 'python'
    return
  }
  Write-Failure 'no way to install rnsd: need uv, pipx, or Python >= 3.9 (winget install astral-sh.uv, or winget install Python.Python.3.12)'
  exit 2
}

function Get-RungDescription {
  switch ($script:InstallRung) {
    'present' { return "reuse $script:ExistingRnsd (already works)" }
    'uv' { return "uv tool install --python 3.12 `"$script:RnsSpec`"" }
    'pipx' { return "pipx install `"$script:RnsSpec`"" }
    'venv' { return "$script:PythonFile $($script:PythonPrefix -join ' ') -m venv `"$script:VenvDir`" && pip install `"$script:RnsSpec`"" }
  }
}

function Get-PipxBinDir {
  $dir = ''
  try {
    $dir = (& pipx environment --value PIPX_BIN_DIR 2>$null | Out-String).Trim()
  } catch {
    $dir = ''
  }
  if (-not $dir) { $dir = $env:PIPX_BIN_DIR }
  if (-not $dir) { $dir = Join-Path $env:USERPROFILE '.local\bin' }
  return $dir
}

function Resolve-RealRnsd {
  param([string]$Expected)
  if (Test-Rnsd -Path $Expected) { return $Expected }
  $onPath = Get-Command rnsd -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($onPath -and $onPath.Source -ne $script:RnsdCmd -and (Test-Rnsd -Path $onPath.Source)) { return $onPath.Source }
  return $Expected
}

function Write-Shim {
  param([string]$Real)
  if ($Real -eq $script:RnsdCmd) {
    Write-Failure "no rnsd found for $script:RnsdCmd to call; the shim would invoke itself"
    exit 2
  }
  New-Item -ItemType Directory -Force -Path $script:BinDirResolved | Out-Null
  $text = "@rem Written by scripts/mesh-relay.ps1 for Coyote mesh; runs the installed rnsd.`r`n@`"$Real`" %*`r`n"
  Write-TextFile -Path $script:RnsdCmd -Text $text -Encoding (Get-OemEncoding)
  Write-Info "Wrote shim $script:RnsdCmd -> $Real"
}

function Install-Rnsd {
  switch ($script:InstallRung) {
    'present' {
      Write-Info "rnsd already installed: $script:ExistingRnsd"
      if ($script:ExistingRnsd -ne $script:RnsdCmd) { Write-Shim -Real $script:ExistingRnsd }
    }
    'uv' {
      Write-Info "Installing $script:RnsSpec with uv (managed CPython 3.12)"
      if ((Invoke-Native -File 'uv' -Arguments @('tool', 'install', '--python', '3.12', $script:RnsSpec)) -ne 0) {
        Write-Failure 'uv tool install failed'
        exit 2
      }
      $uvBin = (& uv tool dir --bin --color never | Out-String).Trim()
      Write-Shim -Real (Resolve-RealRnsd -Expected (Join-Path $uvBin 'rnsd.exe'))
    }
    'pipx' {
      Write-Info "Installing $script:RnsSpec with pipx"
      if ((Invoke-Native -File 'pipx' -Arguments @('install', $script:RnsSpec)) -ne 0) {
        Write-Failure 'pipx install failed'
        exit 2
      }
      Write-Shim -Real (Resolve-RealRnsd -Expected (Join-Path (Get-PipxBinDir) 'rnsd.exe'))
      Write-Info "Hint: 'pipx ensurepath' adds pipx's bin dir to PATH if it is not there yet"
    }
    'venv' {
      $venvRnsd = Join-Path $script:VenvDir 'Scripts\rnsd.exe'
      if (Test-Rnsd -Path $venvRnsd) {
        Write-Info "Reusing venv $script:VenvDir"
      } else {
        # A venv that exists but cannot run rnsd is a failed earlier attempt.
        if (Test-Path -LiteralPath $script:VenvDir) { Remove-Item -LiteralPath $script:VenvDir -Recurse -Force }
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:VenvDir) | Out-Null
        Write-Info "Creating venv $script:VenvDir with $script:PythonFile $($script:PythonPrefix -join ' ')"
        $venvArgs = @($script:PythonPrefix) + @('-m', 'venv', $script:VenvDir)
        if ((Invoke-Native -File $script:PythonFile -Arguments $venvArgs) -ne 0) {
          Write-Failure 'could not create a venv; reinstall Python from python.org with pip included, or install uv'
          exit 2
        }
        Write-Info "Installing $script:RnsSpec into the venv"
        $venvPython = Join-Path $script:VenvDir 'Scripts\python.exe'
        if ((Invoke-Native -File $venvPython -Arguments @('-m', 'pip', 'install', '--quiet', $script:RnsSpec)) -ne 0) {
          Write-Failure 'pip install into the venv failed'
          exit 2
        }
      }
      Write-Shim -Real $venvRnsd
    }
  }

  if (-not (Test-Rnsd -Path $script:RnsdCmd)) {
    Write-Failure "$script:RnsdCmd --version failed after install"
    exit 2
  }
  $versionLine = (& $script:RnsdCmd --version | Select-Object -First 1)
  Write-Info "rnsd ready: $script:RnsdCmd ($versionLine)"

  $pathParts = ($env:Path -split ';') | Where-Object { $_ -ne '' }
  if ($pathParts -notcontains $script:BinDirResolved) {
    Write-Info "Note: $script:BinDirResolved is not in PATH. Add it to your User PATH (System Properties > Environment Variables)"
  }
}

function Get-TaskActionArgument {
  return "/c set `"PYTHONUNBUFFERED=1`" && `"$script:RnsdCmd`" >> `"$script:LogFile`" 2>&1"
}

function Test-LoopbackPort {
  $client = New-Object System.Net.Sockets.TcpClient
  try {
    $pending = $client.BeginConnect('127.0.0.1', $ListenPort, $null, $null)
    if (-not $pending.AsyncWaitHandle.WaitOne(1000)) { return $false }
    $client.EndConnect($pending)
    return $true
  } catch {
    return $false
  } finally {
    $client.Dispose()
  }
}

function Wait-Ready {
  Write-Info "Waiting for rnsd on 127.0.0.1:$ListenPort"
  for ($waited = 0; $waited -lt $ReadyTimeoutSeconds; $waited++) {
    if (Test-LoopbackPort) {
      Write-Info "rnsd is listening on 127.0.0.1:$ListenPort"
      return
    }
    Start-Sleep -Seconds 1
  }
  Write-Failure "rnsd did not open 127.0.0.1:$ListenPort within ${ReadyTimeoutSeconds}s. Logs: $script:LogFile"
  exit 3
}

function Install-Service {
  $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
  $actionArgument = Get-TaskActionArgument
  $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue

  if ($DryRun) {
    if ($existing) {
      Write-Info "Service: Scheduled Task '$TaskName' already registered; nothing to do"
    } else {
      Write-Info "Would register Scheduled Task '$TaskName' for ${user}:"
      Write-Output ''
      Write-Output "  Action:    cmd.exe $actionArgument"
      Write-Output '  Trigger:   at logon'
      Write-Output '  Principal: interactive logon, current user'
      Write-Output '  Settings:  hidden, no execution time limit, start when available'
      Write-Output ''
      Write-Output "  Would run: Register-ScheduledTask -TaskName '$TaskName' ...; Start-ScheduledTask -TaskName '$TaskName'"
    }
    Write-Info "Logs: $script:LogFile"
    return
  }

  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:LogFile) | Out-Null
  if ($existing) {
    Write-Info "Scheduled Task '$TaskName' already registered"
  } else {
    $action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument $actionArgument
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive
    $settings = New-ScheduledTaskSettingsSet -Hidden -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
    Write-Info "Registered Scheduled Task '$TaskName'"
    $existing = Get-ScheduledTask -TaskName $TaskName
  }
  if ($existing.State -eq 'Running') {
    Write-Info "'$TaskName' is already running; not restarted"
  } else {
    Start-ScheduledTask -TaskName $TaskName
    Write-Info "Started '$TaskName'"
  }
  Write-Info "Logs: $script:LogFile"
  Wait-Ready
}

function Write-NextStep {
  Write-Info "Diagnostics: 'rnstatus' attaches to the shared instance and lists its interfaces"
  Write-Info 'Matching coyote config (this is the shipped default; normally nothing to paste):'
  Write-Output ''
  Write-Output @"
mesh:
  interfaces:
    - type: private
      host: 127.0.0.1
      port: $ListenPort
"@
  Write-Output ''
  Write-Info 'Next: start coyote and run `.mesh on`'
}

if ($Help) {
  Write-Usage
  exit 0
}

$isWin = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)
if (-not $isWin) {
  Write-Failure 'this script covers Windows only; on Linux/macOS run scripts/mesh-relay.sh'
  exit 2
}

$script:RelayHost = ''
$script:RelayPort = ''
if ($Relay) {
  if ($Relay -match '^([A-Za-z0-9._-]+):([0-9]{1,5})$' -and [int]$Matches[2] -ge 1 -and [int]$Matches[2] -le 65535) {
    $script:RelayHost = $Matches[1]
    $script:RelayPort = $Matches[2]
  } else {
    Write-Failure "-Relay expects host:port, got '$Relay'"
    exit 1
  }
}

$script:RnsSpec = $DefaultRnsSpec
if ($Version) {
  if ($Version -notmatch '^[0-9][0-9A-Za-z.]*$') {
    Write-Failure "-Version expects an rns version such as $($DefaultRnsSpec.Substring(5))"
    exit 1
  }
  $script:RnsSpec = "rns==$Version"
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($isAdmin -and -not $AllowRoot) {
  Write-Failure 'running as Administrator is a usage error: rnsd, its config and its task belong to your user. Re-run from a normal PowerShell, or pass -AllowRoot.'
  exit 1
}

if (-not $env:USERPROFILE) {
  Write-Failure 'USERPROFILE is not set'
  exit 2
}
$script:RnsDir = Join-Path $env:USERPROFILE '.reticulum'
$script:RnsConfig = Join-Path $script:RnsDir 'config'
if ($env:COYOTE_CONFIG_DIR) {
  $coyoteConfig = $env:COYOTE_CONFIG_DIR
} elseif ($env:XDG_CONFIG_HOME) {
  $coyoteConfig = Join-Path $env:XDG_CONFIG_HOME 'coyote'
} else {
  $coyoteConfig = Join-Path $env:APPDATA 'coyote'
}
$script:VenvDir = Join-Path $coyoteConfig 'mesh\rns-venv'
$script:BinDirResolved = $BinDir
if (-not $script:BinDirResolved) { $script:BinDirResolved = Join-Path $env:LOCALAPPDATA 'coyote\bin' }
if (-not [System.IO.Path]::IsPathRooted($script:BinDirResolved)) {
  $script:BinDirResolved = Join-Path $PWD.ProviderPath $script:BinDirResolved
}
$script:BinDirResolved = [System.IO.Path]::GetFullPath($script:BinDirResolved)
$script:RnsdCmd = Join-Path $script:BinDirResolved 'rnsd.cmd'
$script:LogFile = Join-Path $env:LOCALAPPDATA 'coyote\logs\rnsd.log'

Resolve-InstallRung

Write-Info 'OS: windows'
Write-Info "BIN_DIR: $script:BinDirResolved"
Write-Info "Install: $(Get-RungDescription)"
if (Test-Path -LiteralPath $script:RnsConfig -PathType Leaf) {
  Write-Info "Config: $script:RnsConfig (already there)"
} else {
  Write-Info "Config: $script:RnsConfig (to be written)"
}
if ($NoService) {
  Write-Info 'Service: skipped (-NoService)'
} else {
  Write-Info "Service: Scheduled Task '$TaskName' at logon"
}

if ($DryRun) {
  Write-Info 'Dry run: nothing will be written'
} else {
  Install-Rnsd
}

Write-Config

if (-not $NoService) {
  Install-Service
}

Write-NextStep
exit 0
