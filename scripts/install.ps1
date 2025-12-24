Param(
  [string]$Version = $env:PX_VERSION,
  [string]$InstallDir = $(if ($env:PX_INSTALL_DIR) { $env:PX_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\\bin" }),
  [string]$Repo = $(if ($env:PX_REPO) { $env:PX_REPO } else { "ck-zhang/px-dev" })
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Repo)) {
  throw "PX_REPO is empty"
}

if ([string]::IsNullOrWhiteSpace($Version)) {
  $api = "https://api.github.com/repos/$Repo/releases/latest"
  $resp = Invoke-RestMethod -Uri $api -Headers @{ "Accept" = "application/vnd.github+json" }
  $Version = $resp.tag_name
}

if ([string]::IsNullOrWhiteSpace($Version)) {
  throw "Could not determine latest release tag (set PX_VERSION or -Version)"
}

$arch = $env:PROCESSOR_ARCHITECTURE
$asset = "px-$Version-Windows-$arch.zip"
$shaAsset = "$asset.sha256"
$base = "https://github.com/$Repo/releases/download/$Version"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP ("px-install-" + [guid]::NewGuid().ToString()))
try {
  $zipPath = Join-Path $tmp $asset
  $shaPath = Join-Path $tmp $shaAsset

  Write-Host "px: downloading $asset ($Repo@$Version)"
  Invoke-WebRequest -Uri "$base/$asset" -OutFile $zipPath

  try {
    Invoke-WebRequest -Uri "$base/$shaAsset" -OutFile $shaPath
    $expected = (Get-Content $shaPath | Select-Object -First 1).Split(" ", [System.StringSplitOptions]::RemoveEmptyEntries)[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $zipPath).Hash.ToLower()
    if ($expected -ne $actual) {
      throw "sha256 mismatch for $asset`nexpected: $expected`nactual:   $actual"
    }
  } catch {
    # If checksum isn't available, proceed without verification.
  }

  Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
  $exe = Join-Path $tmp "px.exe"
  if (!(Test-Path $exe)) {
    throw "expected px.exe in archive"
  }
  Copy-Item -Force $exe (Join-Path $InstallDir "px.exe")
  Write-Host "px: installed to $(Join-Path $InstallDir 'px.exe')"

  if (!(Get-Command px -ErrorAction SilentlyContinue)) {
    Write-Host "px: note: add $InstallDir to your PATH"
  }
} finally {
  Remove-Item -Recurse -Force $tmp
}

