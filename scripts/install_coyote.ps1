<#
coyote installer (Windows/PowerShell 5+ and PowerShell 7)

Examples:
  powershell -NoProfile -ExecutionPolicy Bypass -Command "iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/install_coyote.ps1 | iex"
  pwsh -c "irm https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/install_coyote.ps1 | iex -Version vX.Y.Z"

Parameters:
  -Version   <tag>         (default: latest)
  -BinDir    <path>        (default: %LOCALAPPDATA%\coyote\bin on Windows; ~/.local/bin on *nix PowerShell)
  -WithMesh                Also set up the local Reticulum daemon (rnsd) for Coyote mesh

Exits 3 when -WithMesh was given and the mesh setup failed; coyote itself is still installed.
#>

[CmdletBinding()]
param(
  [string]$Version = $env:COYOTE_VERSION,
  [string]$BinDir = $env:BIN_DIR,
  [switch]$WithMesh
)

if ($Version -and $Version -match '^[0-9]') { $Version = "v$Version" }

# Windows PowerShell 5.1 defaults to TLS 1.0 on older Windows; PowerShell 7 already negotiates 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'Dark-Alex-17/coyote'
$MeshRelayBase = "https://raw.githubusercontent.com/$Repo/refs/heads/main/scripts"

function Write-Info($msg) { Write-Information -MessageData "[coyote-install] $msg" -InformationAction Continue }
function Fail($msg) { Write-Error $msg; exit 1 }

function Get-MeshCommand {
  if ($isWin) {
    "powershell -NoProfile -ExecutionPolicy Bypass -Command `"iwr -useb $MeshRelayBase/mesh-relay.ps1 | iex`""
  } else {
    "curl -fsSL $MeshRelayBase/mesh-relay.sh | bash"
  }
}

function Write-MeshPointer([string]$Suffix = '') {
  Write-Info "Coyote mesh needs a local Reticulum daemon; set it up any time with: $(Get-MeshCommand)   (or re-run this installer with -WithMesh)$Suffix"
}

function Test-OwnedByMe([string]$Path) {
  if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
  if ($isWin) {
    $acl = Get-Acl -LiteralPath $Path -ErrorAction SilentlyContinue
    $owner = if ($acl) { $acl.Owner } else { $null }
    $me = [Security.Principal.WindowsIdentity]::GetCurrent().Name
  } else {
    $owner = (Get-Item -LiteralPath $Path -ErrorAction SilentlyContinue).User
    $me = [Environment]::UserName
  }
  return [bool]($owner -and $owner -eq $me)
}

# A checked-out installer's sibling relay script is used only when the invoking user
# owns both the installer and the sibling; otherwise the relay is fetched from the
# pinned URL.
function Find-SiblingRelay([string]$Name) {
  if (-not $PSCommandPath) { return $null }
  if (-not (Test-OwnedByMe $PSCommandPath)) { return $null }
  $sibling = Join-Path (Split-Path -Parent $PSCommandPath) $Name
  if (Test-OwnedByMe $sibling) { return $sibling }
  return $null
}

# Runs the relay script and records its exit code in $script:MeshRc (1 when it could
# not be fetched) with the reason in $script:MeshFailure; the caller decides how loudly
# to report it. The relay's own output stays on the pipeline, so nothing is returned.
function Invoke-MeshRelay {
  $relayName = if ($isWin) { 'mesh-relay.ps1' } else { 'mesh-relay.sh' }
  $relay = Find-SiblingRelay $relayName
  if (-not $relay) {
    $relay = Join-Path $tmp.FullName $relayName
    $url = "$MeshRelayBase/$relayName"
    Write-Info "Fetching $url"
    try {
      Invoke-WebRequest -UseBasicParsing -Headers @{ 'User-Agent' = 'coyote-installer' } -Uri $url -OutFile $relay
    } catch {
      $script:MeshFailure = "failed to download the mesh setup script. $_"
      $script:MeshRc = 1
      return
    }
  }

  $rc = 0
  try {
    if ($isWin) {
      & $relay -BinDir $BinDir
    } else {
      $env:BIN_DIR = $BinDir
      & bash $relay
    }
    $rc = $LASTEXITCODE
  } catch {
    $script:MeshFailure = "mesh setup failed: $_"
    $rc = 1
  }
  if ($rc -ne 0 -and -not $script:MeshFailure) { $script:MeshFailure = "mesh setup exited with code $rc" }
  $script:MeshRc = $rc
}

Add-Type -AssemblyName System.Runtime
$isWin = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Windows)
$isMac = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::OSX)
$isLin = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform([System.Runtime.InteropServices.OSPlatform]::Linux)

$isAdmin = $false
if ($isWin) {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $isAdmin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
} else {
  $isAdmin = ((& id -u | Out-String).Trim() -eq '0')
}
$script:MeshFailure = ''
$script:MeshRc = 0
$script:MeshExplicitRc = 0

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
  $libc = 'musl'
  try { getconf GNU_LIBC_VERSION *> $null; if ($LASTEXITCODE -eq 0) { $libc = 'gnu' } } catch { $null = $_ }
  try { if ((ldd --version 2>&1 | Out-String) -imatch 'glibc') { $libc = 'gnu' } } catch { $null = $_ }
  if ($libc -eq 'gnu') {
    # ldconfig lives in /usr/sbin on Debian/Ubuntu, often missing from non-root
    # PATHs, so try its known locations and fall back to probing library dirs.
    $libssl3 = $false
    foreach ($ldc in @('ldconfig', '/sbin/ldconfig', '/usr/sbin/ldconfig')) {
      try { if ((& $ldc -p 2>&1 | Out-String) -match 'libssl\.so\.3') { $libssl3 = $true; break } } catch { $null = $_ }
    }
    if (-not $libssl3) {
      foreach ($libDir in @('/usr/lib/*/libssl.so.3', '/lib/*/libssl.so.3', '/usr/lib64/libssl.so.3', '/usr/lib/libssl.so.3', '/usr/local/lib/libssl.so.3', '/usr/local/lib/*/libssl.so.3')) {
        if (Get-Item -Path $libDir -ErrorAction SilentlyContinue) { $libssl3 = $true; break }
      }
    }
    if ($libssl3) {
      $candidates += "coyote-$arch-unknown-linux-gnu.tar.gz"
    } else {
      Write-Info "glibc detected but OpenSSL 3 (libssl.so.3) not found; using musl build"
    }
  }
  $candidates += "coyote-$arch-unknown-linux-musl.tar.gz"
} else {
  Fail "Unsupported OS for this installer: $os"
}

$tmp = New-Item -ItemType Directory -Force -Path ([IO.Path]::Combine([IO.Path]::GetTempPath(), "coyote-$(Get-Random)"))

try {
  $exec = if ($isWin) { 'coyote.exe' } else { 'coyote' }
  $dest = Join-Path $BinDir $exec

  $installed = $false
  $tried = @()
  $attempt = 0
  foreach ($c in $candidates) {
    $asset = $release.assets | Where-Object { $_.name -eq $c } | Select-Object -First 1
    if (-not $asset) {
      $tried += "${c}: no matching release asset"
      continue
    }

    $attempt++
    $work = New-Item -ItemType Directory -Force -Path (Join-Path $tmp.FullName "attempt-$attempt")

    Write-Info "Selected asset: $($asset.name)"
    Write-Info "Download URL:  $($asset.browser_download_url)"

    if (-not ([string]$asset.browser_download_url).StartsWith('https://')) {
      Write-Info "Refusing a non-HTTPS download URL for ${c}; trying next candidate"
      $tried += "${c}: download URL is not https"
      continue
    }
    $archive = Join-Path $work.FullName 'asset'
    try {
      Invoke-WebRequest -UseBasicParsing -Headers @{ 'User-Agent' = 'coyote-installer' } -Uri $asset.browser_download_url -OutFile $archive
    } catch {
      Write-Info "Failed to download ${c}; trying next candidate. $_"
      $tried += "${c}: download failed"
      continue
    }

    $extractDir = Join-Path $work.FullName 'extract'; New-Item -ItemType Directory -Force -Path $extractDir | Out-Null

    try {
      if ($asset.name -match '\.zip$') {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $extractDir)
      } elseif ($asset.name -match '\.tar\.gz$' -or $asset.name -match '\.tgz$') {
        $tar = Get-Command tar -ErrorAction SilentlyContinue
        if ($tar) { & $tar.Source -xzf $archive -C $extractDir }
        else { throw "Asset is tar archive but 'tar' is not available." }
      } else {
        try { Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $extractDir) }
        catch {
          $tar = Get-Command tar -ErrorAction SilentlyContinue
          if ($tar) { & $tar.Source -xf $archive -C $extractDir } else { throw "Unknown archive format; neither zip nor tar workable." }
        }
      }
    } catch {
      Write-Info "Failed to extract ${c}; trying next candidate. $_"
      $tried += "${c}: extract failed"
      continue
    }

    $bin = $null
    foreach ($item in Get-ChildItem -Recurse -File $extractDir) {
      if ($item.Name -ieq $exec) { $bin = $item.FullName }
    }
    if (-not $bin) {
      Write-Info "Could not find coyote binary inside ${c}; trying next candidate"
      $tried += "${c}: no coyote binary in archive"
      continue
    }

    if (-not $isWin) { try { & chmod +x -- $bin } catch { $null = $_ } }

    $works = $false
    try { & $bin --version *> $null; if ($LASTEXITCODE -eq 0) { $works = $true } } catch { $null = $_ }
    if (-not $works -and -not $isWin) {
      # The temp dir may live on a noexec mount; retry from a probe file in
      # the install directory before rejecting.
      $probe = Join-Path $BinDir ".coyote-install-probe-$PID"
      try {
        Copy-Item -Force $bin $probe
        & chmod +x -- $probe
        & $probe --version *> $null
        if ($LASTEXITCODE -eq 0) { $works = $true }
      } catch { $null = $_ } finally {
        Remove-Item -Force -ErrorAction SilentlyContinue $probe
      }
    }
    if (-not $works) {
      Write-Info "Downloaded $c but it failed to run on this system; trying next candidate"
      $tried += "${c}: binary failed to run on this system"
      continue
    }

    Copy-Item -Force $bin $dest
    Write-Info "Installed: $dest"
    $installed = $true
    break
  }

  if (-not $installed) {
    Write-Error "No usable asset found for $os-$arch. Tried:"
    $tried | ForEach-Object { Write-Error "  - $_" }
    exit 1
  }

  if ($isWin) {
    $pathParts = ($env:Path -split ';') | Where-Object { $_ -ne '' }
    if ($pathParts -notcontains $BinDir) {
      # Read/write the User PATH via the registry directly: the [Environment]
      # round-trip expands %VAR% entries and bakes them in on write.
      $regKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
      if ($regKey) {
        $userPath = [string]$regKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if (-not (($userPath -split ';') -contains $BinDir)) {
          $newUserPath = if ($userPath.Trim().Length -gt 0) { "$userPath;$BinDir" } else { $BinDir }
          $regKey.SetValue('Path', $newUserPath, [Microsoft.Win32.RegistryValueKind]::ExpandString)
          Write-Info "Added to User PATH: $BinDir (restart shell to take effect)"
        }
        $regKey.Close()
      }
    }
  } else {
    if (-not ($env:PATH -split ':' | Where-Object { $_ -eq $BinDir })) {
      Write-Info "Note: $BinDir is not in PATH. Add it to your shell profile."
    }
  }

  # Mesh is optional on the prompt path, so a failure there is a note and the pointer;
  # an explicit -WithMesh that fails is an error. An rnsd already in BinDir means a
  # mesh set up earlier (an upgrade), so neither the prompt nor the pointer repeats.
  # Administrator is never prompted: rnsd, its config and its task belong to the user.
  $rnsdName = if ($isWin) { 'rnsd.cmd' } else { 'rnsd' }
  if ($WithMesh) {
    Invoke-MeshRelay
    $script:MeshExplicitRc = $script:MeshRc
    if ($script:MeshExplicitRc -ne 0) {
      $retryAs = if ($isAdmin) { ' as your normal user' } else { '' }
      [Console]::Error.WriteLine("[coyote-install] Error: $script:MeshFailure; coyote itself is installed. Retry with: $(Get-MeshCommand)$retryAs")
    }
  } elseif (-not (Test-Path -LiteralPath (Join-Path $BinDir $rnsdName))) {
    if ($isAdmin) {
      Write-MeshPointer ' - run it as your normal user, not as Administrator'
    } elseif ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected -and -not $env:CI) {
      $choice = $Host.UI.PromptForChoice('Coyote mesh', 'Set up the local Reticulum daemon for Coyote mesh now?', @('&Yes', '&No'), 1)
      if ($choice -eq 0) {
        Invoke-MeshRelay
        if ($script:MeshRc -ne 0) {
          Write-Info "$script:MeshFailure; coyote itself is installed."
          Write-MeshPointer
        }
      } else {
        Write-MeshPointer
      }
    } else {
      Write-MeshPointer
    }
  }

  Write-Info "Done. Try: coyote --help"
  if ($script:MeshExplicitRc -ne 0) { exit 3 }
} finally {
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}
