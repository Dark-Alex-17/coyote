<#
Local Reticulum daemon (rnsd) for Coyote mesh (Windows, PowerShell 5.1+ and 7)

Installs rnsd, writes the Reticulum config once, in the directory rnsd reads
(%USERPROFILE%\.config\reticulum when it holds a config, else %USERPROFILE%\.reticulum),
and registers a Scheduled Task that starts the daemon at logon; every Coyote
session on this host then attaches to its shared instance (the shipped
mesh.interfaces default, type: local). On Linux/macOS use scripts/mesh-relay.sh.

Examples:
  pwsh -File scripts\mesh-relay.ps1
  pwsh -File scripts\mesh-relay.ps1 -Relay relay.example.org:4242
  $env:BIN_DIR = "$env:USERPROFILE\bin"; pwsh -File scripts\mesh-relay.ps1 -NoService

Parameters:
  -Relay <host:port>  Also dial your team's relay (TCPClientInterface).
  -Version <X>        rns version to install (default: see -Help).
  -BinDir <path>      Where rnsd.cmd goes (default: %LOCALAPPDATA%\coyote\bin). Or set BIN_DIR.
  -NoService          Skip the Scheduled Task.
  -KeepRnsd           Keep an older rnsd that is already installed instead of upgrading it to the pinned version.
  -DryRun             Print the plan and write nothing.
  -AllowRoot          Permit running as Administrator.

Exit codes: 0 done or nothing to do; 1 usage; 2 unsupported OS or no way to
install rnsd, or an existing config without the 127.0.0.1:4242 listener or with
enable_transport off (no task is registered then); 3 task registered but 127.0.0.1:4242
did not come up in 30 s.
#>

[CmdletBinding()]
param(
  [string]$Relay = '',
  [string]$Version = '',
  [string]$BinDir = $env:BIN_DIR,
  [switch]$NoService,
  [switch]$KeepRnsd,
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
# The override exists for the test harness; operators get the documented 30 s.
$ReadyTimeoutSeconds = if ($env:COYOTE_MESH_READY_TIMEOUT) { [int]$env:COYOTE_MESH_READY_TIMEOUT } else { 30 }
# How long a stopped task's rnsd gets to let go of 127.0.0.1:4242 before the restart is
# given up on; the harness shortens it.
$StopWaitSeconds = 10
# The values ConfigObj's as_bool reads as true, which is how rnsd reads enabled and
# enable_transport. PowerShell's -match is case-insensitive.
$TrueValue = '(yes|true|on|1)(\s|#|$)'

function Write-Usage {
  Write-Output 'coyote mesh relay setup (Windows): install rnsd, write its config, run it as a Scheduled Task'
  Write-Output ''
  Write-Output 'Options:'
  Write-Output '  -Relay <host:port>      Also dial your team''s relay (TCPClientInterface)'
  Write-Output "  -Version <X>            rns version to install (default: $($DefaultRnsSpec.Substring(5)))"
  Write-Output '  -BinDir <path>          Where rnsd.cmd goes (default: %LOCALAPPDATA%\coyote\bin)'
  Write-Output '  -NoService              Skip the Scheduled Task'
  Write-Output '  -KeepRnsd               Keep an older rnsd that is already installed instead of upgrading it to the pinned version'
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
# exit code come back as data instead of terminating errors. Stdout and stderr go to
# the host as plain lines, not the output stream, so the only return value is the exit
# code; stderr is turned to text first, since PowerShell (under remoting above all)
# would otherwise print a line of it as a NativeCommandError record.
function Invoke-Native {
  param([string]$File, [string[]]$Arguments = @(), [switch]$Quiet)
  $ErrorActionPreference = 'Continue'
  try {
    if ($Quiet) { & $File @Arguments *> $null } else { & $File @Arguments 2>&1 | ForEach-Object { "$_" } | Out-Host }
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

  # The TCP hop for a session configured with type: private at 127.0.0.1:4242. It listens on
  # loopback only: for other hosts to dial this one as their relay, set listen_ip to an
  # address they can reach and firewall the port. Sessions on this host attach over the
  # shared instance instead.
  [[Coyote Sessions]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = 4242

    # Reticulum holds announces from unknown peers by default, which would delay a
    # brand-new Coyote identity's first announce. Off here, as on the repo's test relay.
    ingress_control = No
'@
  # The team relay is often the propagation node from deployment/propagation-node. Its
  # lxmd.config announces every 61 minutes, not 60: this rnsd, like every transport node,
  # rate-limits announces per destination (announce_rate_target 3600 s, grace 5, a strict
  # compare) and stops forwarding a node that announces hourly or faster, so a session that
  # joins later would never hear the node through it.
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

# Relay announces and paths between the Coyote sessions attached to it. A transport
# node also forwards traffic for any other Reticulum peer it hears on its interfaces.
enable_transport = True

# Coyote sessions announce hourly (3600 s plus a minute of slack, so the relay never counts
# a heartbeat as early): a transport node rate-limits announces per destination, Reticulum's
# defaults being announce_rate_target = 3600 s with an announce_rate_grace of 5 (RNS
# Interfaces/Interface.py, DEFAULT_AR_TARGET and DEFAULT_AR_GRACE), and past that grace it
# blocks a destination's rebroadcasts until an hour after its last accepted announce. Do not
# lower announce_rate_target here, nor default_ar_target in the [reticulum] section: team
# relays and community nodes keep the default.

# Every Coyote session on this host attaches to this daemon as a shared-instance client
# (the shipped mesh.interfaces default, type: local), as do rnstatus, rnpath, Sideband and
# NomadNet. The three keys below are Reticulum's own defaults, written out so the contract
# is visible. On Linux and Android the shared instance is the abstract AF_UNIX socket
# rns/<instance_name>: instance_name is the key that matters there, and shared_instance_port
# is inert unless shared_instance_type = tcp. On macOS, BSD and Windows the shared instance
# is TCP 127.0.0.1:<shared_instance_port>. Any other local account on this host can attach
# to this daemon too, over the shared instance and 127.0.0.1:4242; that is inherent to
# Reticulum's shared-instance model.
share_instance = Yes
shared_instance_port = 37428
instance_name = default

[logging]

# rnsd applies its -v flags only when this section exists (Reticulum.py:459-467).
# 4 = LOG_INFO, the value RNS's own default config ships.
loglevel = 4

'@
}

function Get-ConfigText {
  $head = @'
# Reticulum configuration written by scripts/mesh-relay.ps1 for Coyote mesh.
# This daemon owns the interfaces; every Coyote session on this host attaches to its
# shared instance (mesh.interfaces: [{type: local}], the shipped default).
# Interface options: https://markqvist.github.io/Reticulum/manual/interfaces.html

'@
  return $head + "`n" + (Get-SettingsText) + "`n" + (Get-InterfaceText) + "`n"
}

function Write-FirewallWarning {
  Write-Info 'Firewall: AutoInterface listens for LAN peers, and with enable_transport this rnsd forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured). Windows will ask whether python/rnsd may accept incoming connections; allow it or discovery of LAN hosts will not work.'
}

# True when the config has an AutoInterface stanza that is not switched off; comments and
# disabled stanzas do not count.
function Test-ConfigEnablesAutoInterface {
  param([string]$Path)
  $inAuto = $false
  $enabled = $true
  foreach ($line in [IO.File]::ReadAllLines($Path)) {
    if ($line -match '^\s*\[\[') {
      if ($inAuto -and $enabled) { return $true }
      $inAuto = $false
      $enabled = $true
    } elseif ($line -match '^\s*type\s*=\s*AutoInterface') {
      $inAuto = $true
    } elseif ($line -match '^\s*(interface_)?enabled\s*=\s*(no|false|0)\b') {
      $enabled = $false
    }
  }
  return ($inAuto -and $enabled)
}

# True for a target_host outside loopback, RFC1918, CGNAT (100.64/10), IPv4 link-local
# and IPv6 ULA and link-local. A name other than localhost counts as public: what it
# resolves to is not this script's to guess.
function Test-PublicHost {
  param([string]$Target)
  if ($Target -eq 'localhost' -or $Target -eq '::1') { return $false }
  if ($Target -match '^f[cd][0-9a-f]{2}:' -or $Target -match '^fe[89ab][0-9a-f]:') { return $false }
  if ($Target -match '^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$') {
    $a = [int]$Matches[1]
    $b = [int]$Matches[2]
    if ($a -eq 10 -or $a -eq 127) { return $false }
    if ($a -eq 172 -and $b -ge 16 -and $b -le 31) { return $false }
    if ($a -eq 192 -and $b -eq 168) { return $false }
    if ($a -eq 100 -and $b -ge 64 -and $b -le 127) { return $false }
    if ($a -eq 169 -and $b -eq 254) { return $false }
  }
  return $true
}

# The existing config read as rnsd would: ServesSessions when an enabled TCPServerInterface
# listens on 127.0.0.1:$ListenPort, Transport when the [reticulum] section turns
# enable_transport on, and PublicDials, the enabled TCPClientInterface stanzas that dial a
# public host. rnsd brings a stanza up only when enabled or interface_enabled is true, so a
# stanza without either stays off; port wins over listen_port, as in RNS TCPInterface.py.
function Read-ExistingConfig {
  param([string]$Path)
  $stanzas = New-Object System.Collections.Generic.List[hashtable]
  $section = ''
  $transport = $false
  $current = @{ Type = ''; Enabled = $false; ListenPort = ''; Port = ''; Target = ''; ListenIp = ''; Device = $false }
  foreach ($line in [IO.File]::ReadAllLines($Path)) {
    if ($line -match '^\s*\[') {
      $stanzas.Add($current)
      $current = @{ Type = ''; Enabled = $false; ListenPort = ''; Port = ''; Target = ''; ListenIp = ''; Device = $false }
      if ($line -match '^\s*\[\[') {
        $section += '/'
      } elseif ($line -match '^\s*\[([^\]]*)\]') {
        $section = $Matches[1]
      }
    } elseif ($section -ceq 'reticulum' -and $line -cmatch "^\s*enable_transport\s*=\s*(?i:$TrueValue)") {
      $transport = $true
    } elseif ($line -match '^\s*type\s*=\s*([A-Za-z]+)') {
      $current.Type = $Matches[1]
    } elseif ($line -cmatch "^\s*(interface_)?enabled\s*=\s*(?i:$TrueValue)") {
      $current.Enabled = $true
    } elseif ($line -match '^\s*listen_port\s*=\s*(\d+)') {
      $current.ListenPort = $Matches[1]
    } elseif ($line -match '^\s*port\s*=\s*(\d+)') {
      $current.Port = $Matches[1]
    } elseif ($line -match '^\s*target_host\s*=\s*([^\s#]+)') {
      $current.Target = $Matches[1]
    } elseif ($line -match '^\s*listen_ip\s*=\s*([^\s#]+)') {
      $current.ListenIp = $Matches[1]
    } elseif ($line -match '^\s*device\s*=') {
      $current.Device = $true
    }
  }
  $stanzas.Add($current)
  $serves = $false
  $public = 0
  $loopbackIps = @('', '127.0.0.1', '0.0.0.0', 'localhost', '::', '::1')
  foreach ($stanza in $stanzas) {
    if (-not $stanza.Enabled) { continue }
    $port = if ($stanza.Port) { $stanza.Port } else { $stanza.ListenPort }
    if ($stanza.Type -eq 'TCPServerInterface' -and $port -eq "$ListenPort" -and -not $stanza.Device -and $loopbackIps -ccontains $stanza.ListenIp) { $serves = $true }
    if ($stanza.Type -eq 'TCPClientInterface' -and $stanza.Target -and (Test-PublicHost -Target $stanza.Target)) { $public++ }
  }
  return @{ ServesSessions = $serves; Transport = $transport; PublicDials = $public }
}

# An rnsd on this config would never open 127.0.0.1:$ListenPort or would not relay between
# sessions, so the run stops before registering a task that could only time out.
function Assert-ConfigServesCoyote {
  param([bool]$ServesSessions, [bool]$Transport)
  $gap = ''
  if (-not $ServesSessions) { $gap = "has no [[Coyote Sessions]] stanza on 127.0.0.1:$ListenPort" }
  if (-not $Transport) {
    if ($gap) { $gap += ' and enable_transport is off' } else { $gap = 'has enable_transport off' }
  }
  if (-not $gap) { return }
  Write-Failure "Your Reticulum config at $script:RnsConfig $gap, so this rnsd would not serve Coyote. Add the settings above (or move the file aside and run this script again), then run it again."
  exit 2
}

# True when the existing config opens with the header Get-ConfigText writes.
function Test-ConfigIsOwn {
  param([string]$Path)
  $header = ((Get-ConfigText) -split "\r?\n")[0]
  $first = @([IO.File]::ReadAllLines($Path) | Select-Object -First 1)
  return ($first.Count -gt 0 -and $first[0] -ceq $header)
}

# The existing config's lines without its [[Team Relay]] stanza (up to the next section
# header), without the comment this script writes above that stanza, and without trailing blank lines.
function Get-ConfigLinesWithoutTeamRelay {
  param([string]$Path)
  $kept = New-Object System.Collections.Generic.List[string]
  $pending = New-Object System.Collections.Generic.List[string]
  $skipping = $false
  foreach ($line in [IO.File]::ReadAllLines($Path)) {
    if ($line -match '^\s*\[\[Team Relay\]\]') {
      $skipping = $true
      $pending.Clear()
      continue
    }
    if ($line -match '^\s*\[') { $skipping = $false }
    if ($skipping) { continue }
    if ($line -match '^\s*$' -or $line -match "^\s*# Your team's relay ") {
      $pending.Add($line)
      continue
    }
    $kept.AddRange($pending)
    $pending.Clear()
    $kept.Add($line)
  }
  return ,$kept.ToArray()
}

# Stopping the task ends only its cmd.exe; the rnsd and python it started keep running
# with the config they read. The restart ends every process whose command line carries
# this rnsd's `--config "<dir>"`, the whole tree, before starting the task again. The
# shell running the line is spared (run as `pwsh -Command "<line>"` its own command line
# carries the same text), and a `'` in the dir is doubled for the single-quoted literal.
function Get-RestartCommand {
  $dir = $script:RnsDir.Replace("'", "''")
  return "Stop-ScheduledTask -TaskName '$TaskName'; Get-CimInstance Win32_Process | Where-Object { `$_.CommandLine -and `$_.CommandLine.Contains('--config `"$dir`"') -and `$_.ProcessId -ne `$PID } | ForEach-Object { taskkill.exe /PID `$_.ProcessId /T /F }; Start-ScheduledTask -TaskName '$TaskName'"
}

# Every process on the host with its id, parent and command line; replaced by the tests.
function Get-ProcessTable {
  return @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, CommandLine)
}

# Ends one process and every process under it; replaced by the tests.
function Invoke-TreeKill {
  param([int]$ProcessId)
  Invoke-Native -File 'taskkill.exe' -Arguments @('/PID', "$ProcessId", '/T', '/F') -Quiet | Out-Null
}

# The ids of the processes running this rnsd: the task's cmd.exe, the rnsd launcher and
# its python child, each found by the `--config "<dir>"` the task passes on. This
# script's own process is never one.
function Select-RnsdProcessId {
  param($Processes)
  $needle = "--config `"$script:RnsDir`""
  return @(@($Processes) | Where-Object {
      $null -ne $_ -and $_.CommandLine -and $_.CommandLine.Contains($needle) -and $_.ProcessId -ne $PID
    } | ForEach-Object { [int]$_.ProcessId } | Sort-Object -Unique)
}

# Of those, the roots: the ones whose parent is not one of them. `taskkill /T` on a root
# ends the processes under it, so no id is killed after its process already ended and
# the id could have been handed to an unrelated process.
function Select-RnsdRootProcessId {
  param($Processes)
  $ids = Select-RnsdProcessId $Processes
  return @(@($Processes) | Where-Object {
      $null -ne $_ -and $ids -contains [int]$_.ProcessId -and -not ($_.PSObject.Properties['ParentProcessId'] -and $ids -contains [int]$_.ParentProcessId)
    } | ForEach-Object { [int]$_.ProcessId } | Sort-Object -Unique)
}

# Ends any rnsd running on this config, its whole process tree, so the task started next
# is the only rnsd and reads the config as it is now. Returns how many trees it ended.
function Invoke-RnsdTreeKill {
  $roots = @(Select-RnsdRootProcessId (Get-ProcessTable))
  foreach ($id in $roots) {
    Invoke-TreeKill -ProcessId $id
  }
  return $roots.Count
}

# -Relay on a config this script wrote: the [[Team Relay]] stanza replaces the one the file
# holds, if any, in one atomic write, and RelayAdded has the service step restart the task.
function Add-TeamRelay {
  $interfaces = Get-InterfaceText
  $stanza = $interfaces.Substring($interfaces.IndexOf("  # Your team's relay"))
  $text = ((Get-ConfigLinesWithoutTeamRelay -Path $script:RnsConfig) -join "`n") + "`n`n" + $stanza + "`n"
  if ($text -ceq [IO.File]::ReadAllText($script:RnsConfig)) {
    Write-Info "$script:RnsConfig already dials ${script:RelayHost}:$script:RelayPort as its [[Team Relay]]; not touched"
    return
  }
  if ($DryRun) {
    Write-Info "Would add [[Team Relay]] -> ${script:RelayHost}:$script:RelayPort to ${script:RnsConfig}:"
    Write-Output ''
    Write-Output $stanza
    Write-Output ''
    return
  }
  Write-TextFile -Path $script:RnsConfig -Text $text
  $script:RelayAdded = $true
}

# Writes to a temp file beside the target and moves it into place, so a crash never leaves a half-written file.
function Write-TextFile {
  param([string]$Path, [string]$Text, [Text.Encoding]$Encoding = (New-Object System.Text.UTF8Encoding($false)))
  $dir = Split-Path -Parent $Path
  $tmp = Join-Path $dir ('.' + (Split-Path -Leaf $Path) + '.' + [IO.Path]::GetRandomFileName())
  try {
    [IO.File]::WriteAllText($tmp, $Text, $Encoding)
    Move-Item -LiteralPath $tmp -Destination $Path -Force
  } catch {
    Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    throw
  }
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
    $stanzas = Read-ExistingConfig -Path $script:RnsConfig
    $own = $stanzas.ServesSessions -and $stanzas.Transport -and (Test-ConfigIsOwn -Path $script:RnsConfig)
    if ($script:RelayHost -and $own) {
      Add-TeamRelay
      return
    }
    if ($own) {
      # The config this script wrote: nothing to merge, so no stanzas to print.
      Write-Info "$script:RnsConfig is the one this script wrote (already there); not touched"
      if ($stanzas.PublicDials -gt 0) {
        $nodes = if ($stanzas.PublicDials -eq 1) { 'node' } else { 'nodes' }
        Write-Info "Note: this config also dials $($stanzas.PublicDials) public Reticulum $nodes; your announces reach them (see mesh.display_name_on_public)."
      }
      if (Test-ConfigEnablesAutoInterface -Path $script:RnsConfig) { Write-FirewallWarning }
      return
    }
    Write-Info "$script:RnsConfig already exists, not touched. Make sure it carries these settings and stanzas:"
    Write-Output ''
    Write-Output (Get-SettingsText)
    Write-Output (Get-InterfaceText)
    Write-Output ''
    if ($stanzas.PublicDials -gt 0) {
      $nodes = if ($stanzas.PublicDials -eq 1) { 'node' } else { 'nodes' }
      Write-Info "Note: this config also dials $($stanzas.PublicDials) public Reticulum $nodes; your announces reach them (see mesh.display_name_on_public)."
    }
    Assert-ConfigServesCoyote -ServesSessions $stanzas.ServesSessions -Transport $stanzas.Transport
    if (Test-ConfigEnablesAutoInterface -Path $script:RnsConfig) { Write-FirewallWarning }
    if ($script:RelayHost) { $script:RelayForHandEdit = $true }
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

# The first X.Y.Z in `<Path> --version`; empty when there is none.
function Get-RnsdVersion {
  param([string]$Path)
  $ErrorActionPreference = 'Continue'
  $line = ''
  try {
    $line = (& $Path --version 2>$null | Select-Object -First 1 | Out-String)
  } catch {
    $line = ''
  }
  if ($line -match '([0-9]+)\.([0-9]+)\.([0-9]+)') { return "$($Matches[1]).$($Matches[2]).$($Matches[3])" }
  return ''
}

# older, equal or newer: the installed X.Y.Z against the pinned X.Y.Z.
function Compare-Version {
  param([string]$Have, [string]$Pin)
  $h = $Have.Split('.')
  $p = $Pin.Split('.')
  for ($i = 0; $i -lt 3; $i++) {
    if ([int]$h[$i] -lt [int]$p[$i]) { return 'older' }
    if ([int]$h[$i] -gt [int]$p[$i]) { return 'newer' }
  }
  return 'equal'
}

function Test-RnsInToolList {
  param([string]$File, [string[]]$Arguments, [string]$Pattern)
  if (-not (Get-Command $File -CommandType Application -ErrorAction SilentlyContinue)) { return $false }
  $ErrorActionPreference = 'Continue'
  try {
    $listing = (& $File @Arguments 2>$null | Out-String)
  } catch {
    return $false
  }
  return ($listing -match $Pattern)
}

function Test-PathUnder {
  param([string]$Path, [string]$Root)
  if (-not $Root) { return $false }
  return $Path.StartsWith($Root.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

# uv, pipx, venv or unknown: the tool that installed the rnsd at $Path, by where the
# binary really lives (our shim's second line is `@"<real rnsd>" %*`), else by asking
# uv and pipx what they manage.
function Get-RnsdOwner {
  param([string]$Path)
  $target = $Path
  if ($Path -eq $script:RnsdCmd) {
    $lines = [IO.File]::ReadAllLines($Path)
    if ($lines.Count -ge 2 -and $lines[1] -match '^@"(.+)" %\*$') { $target = $Matches[1] }
  }
  if ($target -match '\\uv\\tools\\' -or (Test-PathUnder -Path $target -Root $env:UV_TOOL_DIR)) { return 'uv' }
  if ($target -match '\\pipx\\venvs\\' -or ($env:PIPX_HOME -and (Test-PathUnder -Path $target -Root (Join-Path $env:PIPX_HOME 'venvs')))) { return 'pipx' }
  if (Test-PathUnder -Path $target -Root $script:VenvDir) { return 'venv' }
  if (Test-RnsInToolList -File 'uv' -Arguments @('tool', 'list') -Pattern '(?m)^rns ') { return 'uv' }
  if (Test-RnsInToolList -File 'pipx' -Arguments @('list') -Pattern 'package rns ') { return 'pipx' }
  return 'unknown'
}

function Get-UpgradeCommand {
  switch ($script:RnsdOwner) {
    'uv' { return "uv tool install --force --python 3.12 `"$script:RnsSpec`"" }
    'pipx' { return "pipx install --force `"$script:RnsSpec`"" }
    'venv' { return "`"$(Join-Path $script:VenvDir 'Scripts\python.exe')`" -m pip install --quiet `"$script:RnsSpec`"" }
  }
}

function Invoke-OwnerUpgrade {
  switch ($script:RnsdOwner) {
    'uv' { return Invoke-Native -File 'uv' -Arguments @('tool', 'install', '--force', '--python', '3.12', $script:RnsSpec) }
    'pipx' { return Invoke-Native -File 'pipx' -Arguments @('install', '--force', $script:RnsSpec) }
    'venv' { return Invoke-Native -File (Join-Path $script:VenvDir 'Scripts\python.exe') -Arguments @('-m', 'pip', 'install', '--quiet', '--progress-bar', 'off', $script:RnsSpec) }
  }
  return 127
}

# Sets ExistingVersion, VersionVerdict (older, equal, newer or unknown) and, when an
# upgrade is on the table, RnsdOwner, so the Install: line and Install-Rnsd agree.
function Resolve-ExistingVersion {
  $script:ExistingVersion = ''
  $script:VersionVerdict = 'unknown'
  $script:RnsdOwner = ''
  $script:PinVersion = $script:RnsSpec.Substring(5)
  if ($script:InstallRung -ne 'present') { return }
  $script:ExistingVersion = Get-RnsdVersion -Path $script:ExistingRnsd
  if (-not $script:ExistingVersion) { return }
  if ($script:PinVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    Write-Info "Version: $script:RnsSpec is not X.Y.Z, so the installed rnsd $script:ExistingVersion is kept as is"
    return
  }
  $script:VersionVerdict = Compare-Version -Have $script:ExistingVersion -Pin $script:PinVersion
  if ($script:VersionVerdict -eq 'older' -and -not $script:KeepExisting) {
    $script:RnsdOwner = Get-RnsdOwner -Path $script:ExistingRnsd
  }
}

function Get-PresentDescription {
  switch ($script:VersionVerdict) {
    'older' {
      if ($script:KeepExisting) { return "reuse $script:ExistingRnsd (already works; rnsd $script:ExistingVersion kept by -KeepRnsd, the pin is $script:PinVersion)" }
      if ($script:RnsdOwner -eq 'unknown') { return "reuse $script:ExistingRnsd (already works; rnsd $script:ExistingVersion is older than the pin $script:PinVersion and its installer is unknown; upgrade it yourself or pass -KeepRnsd)" }
      return "upgrade rnsd $script:ExistingVersion -> $script:PinVersion via $(Get-UpgradeCommand)"
    }
    'newer' { return "reuse $script:ExistingRnsd (already works; rnsd $script:ExistingVersion is newer than the version this script was tested with, $script:PinVersion)" }
    default { return "reuse $script:ExistingRnsd (already works)" }
  }
}

function Get-RungDescription {
  switch ($script:InstallRung) {
    'present' { return Get-PresentDescription }
    'uv' { return "uv tool install --python 3.12 `"$script:RnsSpec`"" }
    'pipx' { return "pipx install `"$script:RnsSpec`"" }
    'venv' {
      $python = (@($script:PythonFile) + @($script:PythonPrefix)) -join ' '
      return "$python -m venv `"$script:VenvDir`" && pip install `"$script:RnsSpec`""
    }
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
  Write-RnstatusShim -Real $Real
}

# The program a shim this script wrote calls (its `@"<path>" %*` line), or '' for any
# other file.
function Get-ShimTarget {
  param([string]$Path)
  foreach ($line in [IO.File]::ReadAllLines($Path, (Get-OemEncoding))) {
    if ($line -match '^@"(.+)" %\*$') { return $Matches[1] }
  }
  return ''
}

# rnstatus beside the rnsd that $Real names, run with PYTHONUTF8=1: under the default
# cp1252 console it crashes on the arrow in its traffic column (UnicodeEncodeError).
# setlocal keeps the variable out of the console that ran it.
function Write-RnstatusShim {
  param([string]$Real)
  $realRnstatus = Join-Path (Split-Path -Parent $Real) 'rnstatus.exe'
  if (-not (Test-Path -LiteralPath $realRnstatus -PathType Leaf)) {
    Write-Info "No rnstatus.exe beside $Real; $script:RnstatusCmd not written"
    return
  }
  $text = "@rem Written by scripts/mesh-relay.ps1 for Coyote mesh; runs rnstatus with UTF-8 output.`r`n@setlocal`r`n@set `"PYTHONUTF8=1`"`r`n@`"$realRnstatus`" %*`r`n"
  Write-TextFile -Path $script:RnstatusCmd -Text $text -Encoding (Get-OemEncoding)
  Write-Info "Wrote shim $script:RnstatusCmd -> $realRnstatus"
}

# An older rnsd is upgraded through its installer, or kept with the reason; a newer
# one earns one warning.
function Install-RnsdUpgrade {
  switch ($script:VersionVerdict) {
    'older' {
      if ($script:KeepExisting) { return }
      if ($script:RnsdOwner -eq 'unknown') {
        Write-Info "WARNING: rnsd $script:ExistingVersion at $script:ExistingRnsd is older than the pinned $script:PinVersion and this script cannot tell what installed it; upgrade it yourself (for example: uv tool install --force --python 3.12 `"$script:RnsSpec`" or pipx install --force `"$script:RnsSpec`") and re-run, or pass -KeepRnsd"
        return
      }
      Write-Info "Upgrading rnsd $script:ExistingVersion -> $script:PinVersion with $script:RnsdOwner"
      if ((Invoke-OwnerUpgrade) -ne 0) {
        Write-Failure "$script:RnsdOwner upgrade failed"
        exit 2
      }
      $after = Get-RnsdVersion -Path $script:ExistingRnsd
      if ($after -and (Compare-Version -Have $after -Pin $script:PinVersion) -eq 'older') {
        Write-Info "WARNING: rnsd still reports $after after the upgrade; check that $script:ExistingRnsd is the binary $script:RnsdOwner manages"
      }
    }
    'newer' {
      Write-Info "WARNING: rnsd $script:ExistingVersion is newer than the version this script was tested with ($script:PinVersion); continuing"
    }
  }
}

function Install-Rnsd {
  switch ($script:InstallRung) {
    'present' {
      Write-Info "rnsd already installed: $script:ExistingRnsd"
      Install-RnsdUpgrade
      if ($script:ExistingRnsd -ne $script:RnsdCmd) {
        Write-Shim -Real $script:ExistingRnsd
      } elseif (-not (Test-Path -LiteralPath $script:RnstatusCmd -PathType Leaf)) {
        # rnsd.cmd from an earlier run: rnstatus.cmd goes beside the rnsd it calls.
        $target = Get-ShimTarget -Path $script:RnsdCmd
        if ($target) { Write-RnstatusShim -Real $target }
      }
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
        if ((Invoke-Native -File $venvPython -Arguments @('-m', 'pip', 'install', '--quiet', '--progress-bar', 'off', $script:RnsSpec)) -ne 0) {
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
  return "/c set `"PYTHONUNBUFFERED=1`" && `"$script:RnsdCmd`" --config `"$script:RnsDir`" >> `"$script:LogFile`" 2>&1"
}

function Write-TaskPlan([string]$ActionArgument) {
  Write-Output ''
  Write-Output "  Action:    cmd.exe $ActionArgument"
  Write-Output '  Trigger:   at logon'
  Write-Output '  Principal: interactive logon, current user'
  Write-Output '  Settings:  hidden, no execution time limit, start when available'
  Write-Output ''
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

# Before the exit 3: which config the daemon read and the tail of its log.
function Write-ReadinessReport {
  Write-Info "The service starts rnsd with --config $script:RnsDir, so it read $script:RnsConfig"
  if (-not (Test-Path -LiteralPath $script:LogFile -PathType Leaf)) { return }
  Write-Info "Last 10 lines of ${script:LogFile}:"
  Get-Content -LiteralPath $script:LogFile -Tail 10 | Write-Output
  $reported = @(Select-String -LiteralPath $script:LogFile -Pattern 'Configuration loaded from' | Select-Object -Last 1)
  if ($reported.Count -gt 0) {
    Write-Info "The daemon reported: $($reported[0].Line)"
  }
}

function Wait-Ready {
  Write-Info "Waiting for rnsd on 127.0.0.1:$ListenPort"
  $deadline = [DateTime]::UtcNow.AddSeconds($ReadyTimeoutSeconds)
  # Readiness is the relay port this script opens; Coyote itself attaches over the shared instance.
  do {
    if (Test-LoopbackPort) {
      Write-Info "rnsd is listening on 127.0.0.1:$ListenPort"
      return
    }
    $left = ($deadline - [DateTime]::UtcNow).TotalMilliseconds
    if ($left -gt 0) { Start-Sleep -Milliseconds ([int][Math]::Min(1000, $left)) }
  } while ([DateTime]::UtcNow -lt $deadline)
  Write-ReadinessReport
  Write-Failure "rnsd did not open 127.0.0.1:$ListenPort within ${ReadyTimeoutSeconds}s. Logs: $script:LogFile"
  exit 3
}

# True once 127.0.0.1:$ListenPort is closed, polled for up to $StopWaitSeconds.
function Wait-LoopbackPortClosed {
  $deadline = [DateTime]::UtcNow.AddSeconds($StopWaitSeconds)
  do {
    if (-not (Test-LoopbackPort)) { return $true }
    Start-Sleep -Milliseconds 250
  } while ([DateTime]::UtcNow -lt $deadline)
  return $false
}

# Wrapped in a function so tests can replace it: the static call throws off-Windows.
function Get-CurrentUserName {
  return [Security.Principal.WindowsIdentity]::GetCurrent().Name
}

# The whole shape a task is registered with; a stored task is compared on all of it,
# so a trigger or principal that drifted is caught, not only a changed action.
function Get-TaskShape($Action, $Trigger, $Principal, $Settings) {
  return @(
    "execute=$($Action.Execute)", "arguments=$($Action.Arguments)",
    "trigger=$($Trigger.CimClass.CimClassName)", "logon-user=$($Trigger.UserId)",
    "logon-type=$($Principal.LogonType)", "hidden=$($Settings.Hidden)",
    "time-limit=$($Settings.ExecutionTimeLimit)"
  ) -join "`n"
}

function Install-Service {
  $user = Get-CurrentUserName
  $actionArgument = Get-TaskActionArgument
  $action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument $actionArgument
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
  $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive
  $settings = New-ScheduledTaskSettingsSet -Hidden -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable
  $desired = Get-TaskShape $action $trigger $principal $settings
  $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
  $registered = $null
  if ($existing) {
    $actions = @($existing.Actions)
    $triggers = @($existing.Triggers)
    if ($actions.Count -gt 0 -and $triggers.Count -gt 0) {
      $registered = Get-TaskShape $actions[0] $triggers[0] $existing.Principal $existing.Settings
    }
  }
  $unchanged = ($registered -eq $desired)

  if ($DryRun) {
    if ($existing) {
      if ($unchanged) {
        Write-Info "Service: Scheduled Task '$TaskName' already registered with this action; nothing to do"
      } else {
        Write-Info "Note: the service definition changed; apply it with: Set-ScheduledTask -TaskName '$TaskName' -Action (New-ScheduledTaskAction -Execute 'cmd.exe' -Argument '$actionArgument') -Trigger (New-ScheduledTaskTrigger -AtLogOn -User '$user') -Principal (New-ScheduledTaskPrincipal -UserId '$user' -LogonType Interactive) -Settings (New-ScheduledTaskSettingsSet -Hidden -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable)"
        Write-Info "Would update Scheduled Task '$TaskName' for ${user}:"
        Write-TaskPlan $actionArgument
        Write-Output "  Would run: Set-ScheduledTask -TaskName '$TaskName' -Action ... -Trigger ... -Principal ... -Settings ...; Stop-ScheduledTask and Start-ScheduledTask only when it is not running"
      }
    } else {
      Write-Info "Would register Scheduled Task '$TaskName' for ${user}:"
      Write-TaskPlan $actionArgument
      Write-Output "  Would run: Register-ScheduledTask -TaskName '$TaskName' ...; Start-ScheduledTask -TaskName '$TaskName'"
    }
    Write-Info "Logs: $script:LogFile"
    return
  }

  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:LogFile) | Out-Null
  $changed = $false
  if ($existing) {
    if ($unchanged) {
      Write-Info "Scheduled Task '$TaskName' already registered"
    } else {
      # The stored definition is updated like the unit/plist on the other platforms; a
      # running task keeps the old definition until it is restarted, which stays the user's
      # call unless -Relay added a stanza and the restart for it applies the definition too.
      $changed = $true
      Set-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
      Write-Info "Updated Scheduled Task '$TaskName'"
    }
  } else {
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
    Write-Info "Registered Scheduled Task '$TaskName'"
    $existing = Get-ScheduledTask -TaskName $TaskName
  }
  if ($existing.State -eq 'Running') {
    if ($script:RelayAdded) {
      Write-Info "Restarting '$TaskName' so it dials the new [[Team Relay]]..."
      Stop-ScheduledTask -TaskName $TaskName
      # Stopping the task ends cmd.exe; the rnsd it started outlives it with the old config.
      Invoke-RnsdTreeKill | Out-Null
      if (-not (Wait-LoopbackPortClosed)) {
        Write-Info "rnsd is still running from before: 127.0.0.1:$ListenPort stayed open for $StopWaitSeconds s after Stop-ScheduledTask, so '$TaskName' was not started again. End that rnsd and the processes under it with: taskkill.exe /PID (Get-NetTCPConnection -LocalPort $ListenPort -State Listen).OwningProcess /T /F, then run: Start-ScheduledTask -TaskName '$TaskName'"
        Write-Info "Logs: $script:LogFile"
        return
      }
      Start-ScheduledTask -TaskName $TaskName
      $script:RnsdReload = 'restarted'
    } elseif ($changed) {
      Write-Info "Note: the service definition changed; apply it with: $(Get-RestartCommand)"
    } else {
      Write-Info "'$TaskName' is already running; not restarted"
    }
  } else {
    # A task that was only stopped, not its process tree, leaves its rnsd running with the old
    # config; started on top of it, the task would not be the rnsd that serves.
    if ((Invoke-RnsdTreeKill) -gt 0) {
      Write-Info "Ended an rnsd left running on $script:RnsDir by a stopped '$TaskName'"
      Wait-LoopbackPortClosed | Out-Null
    }
    Start-ScheduledTask -TaskName $TaskName
    Write-Info "Started '$TaskName'"
    $script:RnsdReload = 'started'
  }
  Write-Info "Logs: $script:LogFile"
  Wait-Ready
  if ((Get-ScheduledTask -TaskName $TaskName).State -ne 'Running') {
    Write-Info "WARNING: 127.0.0.1:$ListenPort is open but '$TaskName' is not running; something else holds the port"
  }
}

function Write-NextStep {
  if ($script:RelayAdded) {
    $relay = "${script:RelayHost}:$script:RelayPort"
    switch ($script:RnsdReload) {
      'restarted' { Write-Info "Added [[Team Relay]] -> $relay to $script:RnsConfig and restarted '$TaskName'." }
      'started' { Write-Info "Added [[Team Relay]] -> $relay to $script:RnsConfig and started '$TaskName'." }
      default { Write-Info "Added [[Team Relay]] -> $relay to $script:RnsConfig. Restart rnsd so it dials the relay: $(Get-RestartCommand)" }
    }
  }
  $rnstatus = if (Test-Path -LiteralPath $script:RnstatusCmd -PathType Leaf) { $script:RnstatusCmd } else { 'rnstatus' }
  Write-Info "Diagnostics: '$rnstatus' attaches to the shared instance and lists its interfaces; run it in Windows Terminal"
  Write-Info 'Matching coyote config (this is the shipped default; normally nothing to paste):'
  Write-Output ''
  Write-Output @"
mesh:
  interfaces:
    - type: local
"@
  Write-Output ''
  if ($script:RelayForHandEdit) {
    Write-Info "Next: add the [[Team Relay]] stanza above to $script:RnsConfig, then $(Get-RestartCommand)"
  } else {
    Write-Info 'Next: start coyote and run `.mesh on`'
  }
  if ($script:RelayHost) {
    Write-Info 'If that relay also runs a propagation node, add the destination hash of that propagation node (not the relay host:port) to mesh.propagation_nodes in config.yaml; .mesh config show lists the propagation nodes heard, with their hashes.'
  }
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
$script:RelayAdded = $false
$script:RelayForHandEdit = $false
$script:RnsdReload = ''
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
$script:KeepExisting = [bool]$KeepRnsd

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
# rnsd's own order (RNS/Reticulum.py): %USERPROFILE%\.config\reticulum when it holds a
# config, else %USERPROFILE%\.reticulum. Reticulum looks at /etc/reticulum first, which
# on Windows resolves against the current drive's root; that rung is not mirrored here.
$xdgRnsDir = Join-Path $env:USERPROFILE '.config\reticulum'
if (Test-Path -LiteralPath (Join-Path $xdgRnsDir 'config') -PathType Leaf) {
  $script:RnsDir = $xdgRnsDir
  $script:RnsDirReason = "rnsd reads $xdgRnsDir because it already holds a config"
} else {
  $script:RnsDir = Join-Path $env:USERPROFILE '.reticulum'
  $script:RnsDirReason = "rnsd reads $script:RnsDir (no config under $xdgRnsDir)"
}
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
$script:RnstatusCmd = Join-Path $script:BinDirResolved 'rnstatus.cmd'
$script:LogFile = Join-Path $env:LOCALAPPDATA 'coyote\logs\rnsd.log'

Resolve-InstallRung
Resolve-ExistingVersion

Write-Info 'OS: windows'
Write-Info "BIN_DIR: $script:BinDirResolved"
Write-Info "Install: $(Get-RungDescription)"
if (Test-Path -LiteralPath $script:RnsConfig -PathType Leaf) {
  Write-Info "Config: $script:RnsConfig (already there)"
} else {
  Write-Info "Config: $script:RnsConfig (to be written)"
}
Write-Info "Config dir: $script:RnsDirReason"
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
