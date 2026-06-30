<#
coyote installer (Windows/PowerShell 5+ and PowerShell 7)

Examples:
  powershell -NoProfile -ExecutionPolicy Bypass -Command "iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/coyote/main/scripts/install_coyote.ps1 | iex"
  pwsh -c "irm https://raw.githubusercontent.com/Dark-Alex-17/coyote/main/scripts/install_coyote.ps1 | iex -Version vX.Y.Z"

Parameters:
  -Version   <tag>         (default: latest)
  -BinDir    <path>        (default: %LOCALAPPDATA%\coyote\bin on Windows; ~/.local/bin on *nix PowerShell)
#>

[CmdletBinding()]
param(
  [string]$Version = $env:COYOTE_VERSION,
  [string]$BinDir = $env:BIN_DIR
)

$Repo = 'Dark-Alex-17/coyote'

function Write-Info($msg) { Write-Host "[coyote-install] $msg" }
function Fail($msg) { Write-Error $msg; exit 1 }

Add-Type -AssemblyName System.Runtime
$isWin = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)
$isMac = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)
$isLin = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Linux)

if ($isWin) { $os = 'windows' }
elseif ($isMac) { $os = 'darwin' }
elseif ($isLin) { $os = 'linux' }
else { Fail "Unsupported OS" }

switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
  'X64'  { $arch = 'x86_64' }
  'Arm64'{ $arch = 'aarch64' }
  default { Fail "Unsupported arch: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
}

if (-not $BinDir) {
  if ($isWin) { $BinDir = Join-Path $env:LOCALAPPDATA 'coyote\bin' }
  else { $userHome = $env:HOME; if (-not $userHome) { $userHome = (Get-Item -Path ~).FullName }; $BinDir = Join-Path $userHome '.local/bin' }
}
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Write-Info "Target: $os-$arch"

$apiBase = "https://api.github.com/repos/$Repo/releases"
$relUrl = if ($Version) { "$apiBase/tags/$Version" } else { "$apiBase/latest" }
Write-Info "Fetching release: $relUrl"
try {
  $release = Invoke-RestMethod -UseBasicParsing -Headers @{ 'User-Agent' = 'coyote-installer' } -Uri $relUrl -Method GET
} catch { Fail "Failed to fetch release metadata. $_" }
if (-not $release.assets) { Fail "No assets found in the release." }

$candidates = @()
if ($os -eq 'windows') {
  if ($arch -eq 'x86_64') { $candidates += 'coyote-x86_64-pc-windows-msvc.zip' }
  else { $candidates += 'coyote-aarch64-pc-windows-msvc.zip' }
} elseif ($os -eq 'darwin') {
  if ($arch -eq 'x86_64') { $candidates += 'coyote-x86_64-apple-darwin.tar.gz' }
  else { $candidates += 'coyote-aarch64-apple-darwin.tar.gz' }
} elseif ($os -eq 'linux') {
  if ($arch -eq 'x86_64') {
    $candidates += 'coyote-x86_64-unknown-linux-gnu.tar.gz'
    $candidates += 'coyote-x86_64-unknown-linux-musl.tar.gz'
  } else {
    $candidates += 'coyote-aarch64-unknown-linux-musl.tar.gz'
  }
} else {
  Fail "Unsupported OS for this installer: $os"
}

$asset = $null
foreach ($c in $candidates) {
  $asset = $release.assets | Where-Object { $_.name -eq $c } | Select-Object -First 1
  if ($asset) { break }
}
if (-not $asset) {
  Write-Error "No matching asset found for $os-$arch. Tried:"; $candidates | ForEach-Object { Write-Error "  - $_" }
  exit 1
}

Write-Info "Selected asset: $($asset.name)"
Write-Info "Download URL:  $($asset.browser_download_url)"

$tmp = New-Item -ItemType Directory -Force -Path ([IO.Path]::Combine([IO.Path]::GetTempPath(), "coyote-$(Get-Random)"))
$archive = Join-Path $tmp.FullName 'asset'
try { Invoke-WebRequest -UseBasicParsing -Headers @{ 'User-Agent' = 'coyote-installer' } -Uri $asset.browser_download_url -OutFile $archive } catch { Fail "Failed to download asset. $_" }

$extractDir = Join-Path $tmp.FullName 'extract'; New-Item -ItemType Directory -Force -Path $extractDir | Out-Null

if ($asset.name -match '\.zip$') {
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $extractDir)
} elseif ($asset.name -match '\.tar\.gz$' -or $asset.name -match '\.tgz$') {
  $tar = Get-Command tar -ErrorAction SilentlyContinue
  if ($tar) { & $tar.Source -xzf $archive -C $extractDir }
  else { Fail "Asset is tar archive but 'tar' is not available." }
} else {
  try { Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $extractDir) }
  catch {
    $tar = Get-Command tar -ErrorAction SilentlyContinue
    if ($tar) { & $tar.Source -xf $archive -C $extractDir } else { Fail "Unknown archive format; neither zip nor tar workable." }
  }
}

$bin = $null
Get-ChildItem -Recurse -File $extractDir | ForEach-Object {
  if ($isWin) { if ($_.Name -ieq 'coyote.exe') { $bin = $_.FullName } }
  else { if ($_.Name -ieq 'coyote') { $bin = $_.FullName } }
}
if (-not $bin) { Fail "Could not find coyote binary inside the archive." }

if (-not $isWin) { try { & chmod +x -- $bin } catch {} }

$exec = if ($isWin) { 'coyote.exe'} else { 'coyote' }
$dest = Join-Path $BinDir $exec
Copy-Item -Force $bin $dest
Write-Info "Installed: $dest"

if ($isWin) {
  $pathParts = ($env:Path -split ';') | Where-Object { $_ -ne '' }
  if ($pathParts -notcontains $BinDir) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User'); if (-not $userPath) { $userPath = '' }
    if (-not ($userPath -split ';' | Where-Object { $_ -eq $BinDir })) {
      $newUserPath = if ($userPath.Trim().Length -gt 0) { "$userPath;$BinDir" } else { $BinDir }
      [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
      Write-Info "Added to User PATH: $BinDir (restart shell to take effect)"
    }
  }
} else {
  if (-not ($env:PATH -split ':' | Where-Object { $_ -eq $BinDir })) {
    Write-Info "Note: $BinDir is not in PATH. Add it to your shell profile."
  }
}

Write-Info "Done. Try: coyote --help"

