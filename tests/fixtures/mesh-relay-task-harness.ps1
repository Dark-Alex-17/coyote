<#
Drives mesh-relay.ps1's Install-Service without Windows: only the relay's function
definitions are loaded (none of its top-level code runs), and the ScheduledTask
cmdlets are shadowed by functions over a JSON store under the scenario's temp dir.
The process table and taskkill are shadowed too. Every shadow appends one line to the
calls log. A few scenarios drive another of the relay's functions instead (Write-Config
on a config already there, Invoke-Native, the rnstatus shim). Run by
tests/mesh_relay_task.rs.
#>

param(
  [Parameter(Mandatory)][string]$Script,
  [Parameter(Mandatory)][string]$Store,
  [Parameter(Mandatory)][string]$Calls,
  [Parameter(Mandatory)][string]$Scenario
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Script, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) { throw "parse errors in ${Script}: $($parseErrors -join "`n")" }
$functions = $ast.FindAll({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $false)
foreach ($fn in $functions) {
  . ([scriptblock]::Create($fn.Extent.Text))
}

$root = Split-Path -Parent $Store
# PSScriptAnalyzer counts a parameter as used only at the top level, not inside Add-Call.
$callsLog = $Calls
$script:TaskName = 'Coyote rnsd'
$script:DryRun = $false
$script:ListenPort = 4242
$script:RnsdCmd = Join-Path (Join-Path $root 'bin') 'rnsd.cmd'
$script:RnsDir = Join-Path $root '.reticulum'
$script:LogFile = Join-Path (Join-Path $root 'logs') 'rnsd.log'
$script:RelayAdded = $false
$script:RnsdReload = ''
$script:RelayHost = 'relay.example'
$script:RelayPort = '4242'
$script:RelayForHandEdit = $false
$script:RnsConfig = Join-Path $script:RnsDir 'config'
# The relay's own top-level constants the functions driven here read.
$script:TrueValue = '(yes|true|on|1)(\s|#|$)'
$script:StopWaitSeconds = 1
$script:PortStaysOpen = $false
$script:RnstatusCmd = Join-Path (Join-Path $root 'bin') 'rnstatus.cmd'
$script:BinDirResolved = Join-Path $root 'bin'
$script:Processes = @()
$testUser = 'TESTHOST\tester'

function Add-Call([string]$Line) {
  Add-Content -LiteralPath $callsLog -Value $Line
}

function Get-CurrentUserName {
  return $testUser
}

function Wait-Ready {
  Add-Call 'Wait-Ready'
}

# The port probe answers from the scenario, never from a real listener on this host.
function Test-LoopbackPort {
  Add-Call 'Test-LoopbackPort'
  return $script:PortStaysOpen
}

# The host's processes, from the scenario; a real process table is never read.
function Get-ProcessTable {
  Add-Call 'Get-ProcessTable'
  return $script:Processes
}

# taskkill /T /F: the process and every process under it end.
function Invoke-TreeKill {
  [CmdletBinding(SupportsShouldProcess)]
  param([int]$ProcessId)
  Add-Call "Invoke-TreeKill $ProcessId"
  if ($PSCmdlet.ShouldProcess("$ProcessId")) {
    $ended = @($ProcessId)
    do {
      $more = @($script:Processes | Where-Object { $ended -notcontains $_.ProcessId -and $ended -contains $_.ParentProcessId } | ForEach-Object { $_.ProcessId })
      $ended += $more
    } while ($more.Count -gt 0)
    $script:Processes = @($script:Processes | Where-Object { $ended -notcontains $_.ProcessId })
  }
}

# The rnsd processes left on this config once the run is over, as the calls log's last line.
function Add-RnsdProcessCall {
  Add-Call "rnsd processes: $((Select-RnsdProcessId $script:Processes) -join ' ')"
}

# The processes the running task left: its cmd.exe, the rnsd launcher and the python it
# started, each with the task's --config, beside an rnsd of another config and this
# harness's own process, whose command line names the same config.
function Initialize-RunningRnsdProcess {
  $config = "--config `"$script:RnsDir`""
  $script:Processes = @(
    [pscustomobject]@{ ProcessId = 100; ParentProcessId = 50; CommandLine = "cmd.exe /c set `"PYTHONUNBUFFERED=1`" && `"$script:RnsdCmd`" $config >> log 2>&1" },
    [pscustomobject]@{ ProcessId = 101; ParentProcessId = 100; CommandLine = "`"C:\venv\Scripts\rnsd.exe`" $config" },
    [pscustomobject]@{ ProcessId = 102; ParentProcessId = 101; CommandLine = "`"C:\Python312\python.exe`" `"C:\venv\Scripts\rnsd.exe`" $config" },
    [pscustomobject]@{ ProcessId = 200; ParentProcessId = 50; CommandLine = '"C:\other\rnsd.exe" --config "C:\other\.reticulum"' },
    [pscustomobject]@{ ProcessId = $PID; ParentProcessId = 50; CommandLine = "pwsh -File harness.ps1 $config" },
    [pscustomobject]@{ ProcessId = 300; ParentProcessId = 50; CommandLine = $null }
  )
}

# PSScriptAnalyzer wants SupportsShouldProcess on New-/Set-/Register-/Start- verbs and,
# once declared, a ShouldProcess call; nothing here ever passes -WhatIf.
function New-ScheduledTaskAction {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$Execute, [string]$Argument)
  if ($PSCmdlet.ShouldProcess($Execute)) {
    return [pscustomobject]@{ Execute = $Execute; Arguments = $Argument }
  }
}

function New-ScheduledTaskTrigger {
  [CmdletBinding(SupportsShouldProcess)]
  param([switch]$AtLogOn, [string]$User)
  $class = if ($AtLogOn) { 'MSFT_TaskLogonTrigger' } else { 'MSFT_TaskTrigger' }
  if ($PSCmdlet.ShouldProcess($class)) {
    return [pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = $class }; UserId = $User }
  }
}

function New-ScheduledTaskPrincipal {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$UserId, [string]$LogonType)
  if ($PSCmdlet.ShouldProcess($UserId)) {
    return [pscustomobject]@{ UserId = $UserId; LogonType = $LogonType }
  }
}

# ExecutionTimeLimit is stored as ISO 8601 ('PT0S'), as Windows does.
function New-ScheduledTaskSettingsSet {
  [CmdletBinding(SupportsShouldProcess)]
  param([switch]$Hidden, [TimeSpan]$ExecutionTimeLimit, [switch]$StartWhenAvailable)
  if ($PSCmdlet.ShouldProcess('settings')) {
    return [pscustomobject]@{
      Hidden = [bool]$Hidden
      ExecutionTimeLimit = [System.Xml.XmlConvert]::ToString($ExecutionTimeLimit)
      StartWhenAvailable = [bool]$StartWhenAvailable
    }
  }
}

function Read-Store {
  if (-not (Test-Path -LiteralPath $Store)) { return $null }
  return Get-Content -Raw -LiteralPath $Store | ConvertFrom-Json
}

function Write-Store($Task) {
  ConvertTo-Json -InputObject $Task -Depth 6 | Set-Content -LiteralPath $Store
}

function Get-BoundPartList($Bound) {
  return (@('Action', 'Trigger', 'Principal', 'Settings') | Where-Object { $Bound.ContainsKey($_) } | ForEach-Object { "-$_" }) -join ' '
}

function Get-ScheduledTask {
  [CmdletBinding()]
  param([string]$TaskName)
  Add-Call "Get-ScheduledTask '$TaskName'"
  $task = Read-Store
  if ($task -and $task.TaskName -eq $TaskName) { return $task }
  return $null
}

function Register-ScheduledTask {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$TaskName, $Action, $Trigger, $Principal, $Settings)
  Add-Call "Register-ScheduledTask $(Get-BoundPartList $PSBoundParameters)"
  if ($PSCmdlet.ShouldProcess($TaskName)) {
    Write-Store @{
      TaskName = $TaskName; State = 'Ready'
      Actions = @($Action); Triggers = @($Trigger); Principal = $Principal; Settings = $Settings
    }
  }
}

function Set-ScheduledTask {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$TaskName, $Action, $Trigger, $Principal, $Settings)
  Add-Call "Set-ScheduledTask $(Get-BoundPartList $PSBoundParameters)"
  if ($PSCmdlet.ShouldProcess($TaskName)) {
    $task = Read-Store
    Write-Store @{
      TaskName = $TaskName; State = $task.State
      Actions = @($Action); Triggers = @($Trigger); Principal = $Principal; Settings = $Settings
    }
  }
}

function Start-ScheduledTask {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$TaskName)
  Add-Call "Start-ScheduledTask '$TaskName'"
  if ($PSCmdlet.ShouldProcess($TaskName)) {
    $task = Read-Store
    $task.State = 'Running'
    Write-Store $task
    # The task's rnsd, reading the config as it is when the task starts.
    $dials = 'no [[Team Relay]]'
    if ((Test-Path -LiteralPath $script:RnsConfig) -and ([IO.File]::ReadAllText($script:RnsConfig) -match 'target_host = (\S+)')) { $dials = $Matches[1] }
    $script:Processes = @($script:Processes) + @([pscustomobject]@{ ProcessId = 900; ParentProcessId = 50; CommandLine = "`"C:\venv\Scripts\rnsd.exe`" --config `"$script:RnsDir`"" })
    Add-Call "rnsd 900 started, dialing $dials"
  }
}

function Stop-ScheduledTask {
  [CmdletBinding(SupportsShouldProcess)]
  param([string]$TaskName)
  Add-Call "Stop-ScheduledTask '$TaskName'"
  if ($PSCmdlet.ShouldProcess($TaskName)) {
    $task = Read-Store
    $task.State = 'Ready'
    Write-Store $task
  }
}

# The stored task before Install-Service runs: the desired shape, except where the
# scenario's arguments deviate from it.
function Initialize-Store([string]$State, [string]$Arguments, [string]$TriggerClass) {
  Write-Store @{
    TaskName = $TaskName; State = $State
    Actions = @(@{ Execute = 'cmd.exe'; Arguments = $Arguments })
    Triggers = @(@{ CimClass = @{ CimClassName = $TriggerClass }; UserId = $testUser })
    Principal = @{ UserId = $testUser; LogonType = 'Interactive' }
    Settings = @{ Hidden = $true; ExecutionTimeLimit = 'PT0S'; StartWhenAvailable = $true }
  }
}

# A config already there, written by this script (own) or by hand (foreign), and a run
# without -Relay: Write-Config alone.
if ($Scenario -in @('own-config-rerun', 'foreign-config-rerun')) {
  $script:RelayHost = ''
  $script:RelayPort = ''
  New-Item -ItemType Directory -Force -Path $script:RnsDir | Out-Null
  $text = Get-ConfigText
  if ($Scenario -eq 'foreign-config-rerun') { $text = "# my own notes`n" + $text }
  [IO.File]::WriteAllText($script:RnsConfig, $text)
  (Get-Item -LiteralPath $script:RnsConfig).LastWriteTimeUtc = [DateTime]::new(2000, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)
  Write-Config
  exit 0
}

# A native command that writes to stderr and fails: its stderr comes back as plain text
# on the host, and its exit code as the return value.
if ($Scenario -eq 'native-stderr') {
  $pwshExe = (Get-Process -Id $PID).Path
  $code = Invoke-Native -File $pwshExe -Arguments @('-NoProfile', '-NonInteractive', '-Command', "[Console]::Error.WriteLine('Collecting rns 42%'); exit 3")
  Write-Output "exit $code"
  exit 0
}

# rnstatus.cmd beside rnsd.cmd: written from the rnsd the shim calls, and read back from
# an rnsd.cmd an earlier run wrote.
if ($Scenario -eq 'rnstatus-shim') {
  $real = Join-Path $root 'venv Scripts'
  New-Item -ItemType Directory -Force -Path $real, (Split-Path -Parent $script:RnsdCmd) | Out-Null
  $rnsd = Join-Path $real 'rnsd.exe'
  [IO.File]::WriteAllText($rnsd, '')
  [IO.File]::WriteAllText((Join-Path $real 'rnstatus.exe'), '')
  Write-Shim -Real $rnsd
  Write-Output "target $(Get-ShimTarget -Path $script:RnsdCmd)"
  exit 0
}

$desiredArguments = Get-TaskActionArgument
switch ($Scenario) {
  'fresh' { }
  'unchanged-running' { Initialize-Store 'Running' $desiredArguments 'MSFT_TaskLogonTrigger' }
  'unchanged-running-relay-added' {
    Initialize-Store 'Running' $desiredArguments 'MSFT_TaskLogonTrigger'
    $script:RelayAdded = $true
    Initialize-RunningRnsdProcess
  }
  'changed-arguments-running' { Initialize-Store 'Running' '/c stale' 'MSFT_TaskLogonTrigger' }
  'changed-arguments-running-relay-added' {
    Initialize-Store 'Running' '/c stale' 'MSFT_TaskLogonTrigger'
    $script:RelayAdded = $true
  }
  'stale-ready-orphan-relay-added' {
    Initialize-Store 'Ready' $desiredArguments 'MSFT_TaskLogonTrigger'
    $script:RelayAdded = $true
    Initialize-RunningRnsdProcess
    $script:Processes = @($script:Processes | Where-Object { $_.ProcessId -ne 100 })
  }
  'relay-edit-restart' {
    # A config this script wrote, then a run with -Relay: Write-Config edits it, and the
    # service step restarts the running task.
    Initialize-Store 'Running' $desiredArguments 'MSFT_TaskLogonTrigger'
    Initialize-RunningRnsdProcess
    $script:RelayHost = ''
    $script:RelayPort = ''
    New-Item -ItemType Directory -Force -Path $script:RnsDir | Out-Null
    [IO.File]::WriteAllText($script:RnsConfig, (Get-ConfigText))
    $script:RelayHost = 'relay.example'
    $script:RelayPort = '4242'
    Write-Config
    Add-Call "config dials $(if ([IO.File]::ReadAllText($script:RnsConfig) -match 'target_host = (\S+)') { $Matches[1] } else { 'nothing' })"
  }
  'unchanged-running-relay-added-port-stays-open' {
    Initialize-Store 'Running' $desiredArguments 'MSFT_TaskLogonTrigger'
    $script:RelayAdded = $true
    $script:PortStaysOpen = $true
  }
  'changed-trigger-running' { Initialize-Store 'Running' $desiredArguments 'MSFT_TaskDailyTrigger' }
  'stale-ready' { Initialize-Store 'Ready' '/c stale' 'MSFT_TaskLogonTrigger' }
  default { throw "unknown scenario '$Scenario'" }
}

Install-Service
if ($script:RelayAdded) { Write-NextStep }
Add-RnsdProcessCall
exit 0
