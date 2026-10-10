<#
Drives mesh-relay.ps1's Install-Service without Windows: only the relay's function
definitions are loaded (none of its top-level code runs), and the ScheduledTask
cmdlets are shadowed by functions over a JSON store under the scenario's temp dir.
Every shadow appends one line to the calls log. Run by tests/mesh_relay_task.rs.
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
$script:LogFile = Join-Path (Join-Path $root 'logs') 'rnsd.log'
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

$desiredArguments = Get-TaskActionArgument
switch ($Scenario) {
  'fresh' { }
  'unchanged-running' { Initialize-Store 'Running' $desiredArguments 'MSFT_TaskLogonTrigger' }
  'changed-arguments-running' { Initialize-Store 'Running' '/c stale' 'MSFT_TaskLogonTrigger' }
  'changed-trigger-running' { Initialize-Store 'Running' $desiredArguments 'MSFT_TaskDailyTrigger' }
  'stale-ready' { Initialize-Store 'Ready' '/c stale' 'MSFT_TaskLogonTrigger' }
  default { throw "unknown scenario '$Scenario'" }
}

Install-Service
exit 0
