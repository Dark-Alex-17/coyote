<#
Black-box probes of mesh-relay.ps1's Windows-only paths, run without Windows: only the
relay's function definitions are loaded (none of its top-level code runs). Scenarios:

  printed-restart   runs the restart line the script prints (Get-RestartCommand) with
                    the ScheduledTask cmdlets, Get-CimInstance and taskkill.exe shadowed,
                    for the config dir passed in -RnsDir.
  present-rerun     a re-run on a host whose rnsd.cmd an earlier run wrote (before
                    rnstatus.cmd existed): Install-Rnsd's 'present' rung, then
                    Write-NextStep's Diagnostics line.
  present-no-rnstatus  the same, with no rnstatus.exe beside the real rnsd.

Run by tests/mesh_relay_windows_probe.rs.
#>

param(
  [Parameter(Mandatory)][string]$Script,
  [Parameter(Mandatory)][string]$Root,
  [Parameter(Mandatory)][string]$Scenario,
  [string]$RnsDir = ''
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

$script:TaskName = 'Coyote rnsd'
$script:DryRun = $false
$script:ListenPort = 4242
$script:BinDirResolved = Join-Path $Root 'bin'
$script:RnsdCmd = Join-Path $script:BinDirResolved 'rnsd.cmd'
$script:RnstatusCmd = Join-Path $script:BinDirResolved 'rnstatus.cmd'
$script:RnsDir = if ($RnsDir) { $RnsDir } else { Join-Path $Root '.reticulum' }
# A Windows path in -RnsDir has no drive here, so no Join-Path.
$script:RnsConfig = $script:RnsDir + [IO.Path]::DirectorySeparatorChar + 'config'
$script:LogFile = Join-Path (Join-Path $Root 'logs') 'rnsd.log'
$script:RelayHost = ''
$script:RelayPort = ''
$script:RelayAdded = $false
$script:RelayForHandEdit = $false
$script:RnsdReload = ''
$TaskName = $script:TaskName

if ($Scenario -eq 'printed-restart') {
  $line = Get-RestartCommand
  Write-Output "LINE $line"
  $needle = "--config `"$script:RnsDir`""
  $script:ProbeProcesses = @(
    [pscustomobject]@{ ProcessId = 101; CommandLine = "`"C:\venv\Scripts\rnsd.exe`" $needle" },
    [pscustomobject]@{ ProcessId = 102; CommandLine = "`"C:\Python312\python.exe`" `"C:\venv\Scripts\rnsd.exe`" $needle" },
    [pscustomobject]@{ ProcessId = 200; CommandLine = '"C:\other\rnsd.exe" --config "C:\other\.reticulum"' },
    [pscustomobject]@{ ProcessId = 300; CommandLine = $null },
    # The shell running the pasted line: run as `pwsh -Command "<line>"`, its own command
    # line carries the same --config text.
    [pscustomobject]@{ ProcessId = $PID; CommandLine = "pwsh -Command Stop-ScheduledTask ... $needle ..." }
  )
  # PSScriptAnalyzer wants SupportsShouldProcess on Stop-/Start- verbs; nothing passes -WhatIf.
  function Stop-ScheduledTask {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$TaskName)
    if ($PSCmdlet.ShouldProcess($TaskName)) { Write-Output "CALL Stop-ScheduledTask $TaskName" }
  }
  function Start-ScheduledTask {
    [CmdletBinding(SupportsShouldProcess)]
    param([string]$TaskName)
    if ($PSCmdlet.ShouldProcess($TaskName)) { Write-Output "CALL Start-ScheduledTask $TaskName" }
  }
  # Set through the function: drive, since PSScriptAnalyzer refuses a function that shadows
  # a built-in cmdlet by name.
  Set-Item -Path function:Get-CimInstance -Value {
    param([string]$ClassName)
    if ($ClassName -ne 'Win32_Process') { throw "unexpected class $ClassName" }
    $script:ProbeProcesses
  }
  function taskkill.exe { Write-Output "CALL taskkill $($args -join ' ')" }
  try {
    # The line as a user pastes it: parsed from its text, run in a child scope.
    & ([scriptblock]::Create($line))
  } catch {
    Write-Output "ERROR $($_.Exception.Message)"
  }
  exit 0
}

if ($Scenario -in @('present-rerun', 'present-no-rnstatus')) {
  $real = Join-Path $Root 'venv Scripts'
  New-Item -ItemType Directory -Force -Path $real, $script:BinDirResolved | Out-Null
  # The real rnsd is a batch file that prints a version: Install-Rnsd ends by running the
  # shim, and on Windows an empty rnsd.exe would be launched as a program and can hang.
  # The rnstatus.exe beside it is only looked for, never run.
  $realRnsd = Join-Path $real 'rnsd.bat'
  [IO.File]::WriteAllText($realRnsd, "@echo rnsd 1.5.2`r`n")
  if ($Scenario -eq 'present-rerun') { [IO.File]::WriteAllText((Join-Path $real 'rnstatus.exe'), '') }
  # The rnsd.cmd an earlier run (before rnstatus.cmd) wrote.
  $old = "@rem Written by scripts/mesh-relay.ps1 for Coyote mesh; runs the installed rnsd.`r`n@`"$realRnsd`" %*`r`n"
  [IO.File]::WriteAllText($script:RnsdCmd, $old)
  $script:InstallRung = 'present'
  $script:ExistingRnsd = $script:RnsdCmd
  function Install-RnsdUpgrade { }
  function Test-Rnsd { param([string]$Path) return [bool]$Path }
  # Install-Rnsd ends by running the .cmd shim for its version line, which only Windows
  # can; everything this probe judges happens before that.
  try { Install-Rnsd } catch { Write-Output "(version line skipped: $($_.Exception.GetType().Name))" }
  Write-NextStep
  exit 0
}

throw "unknown scenario $Scenario"
