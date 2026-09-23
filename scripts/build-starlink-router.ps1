param(
  [string]$OutputRoot = 'D:\gpt\starlink-dimension-router-release'
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$targetDir = 'D:\gpt\starlink-router-cargo-target'
$env:CARGO_TARGET_DIR = $targetDir
$env:TEMP = 'D:\gpt'
$env:TMP = 'D:\gpt'
$manifest = Join-Path $repoRoot 'starlink-dimension-router\Cargo.toml'
$cargo = 'C:\Users\StarLink\.cargo\bin\cargo.exe'
$releaseDir = Join-Path $OutputRoot 'release'

New-Item -ItemType Directory -Force -Path $OutputRoot, $releaseDir | Out-Null
& $cargo build --release --manifest-path $manifest --offline
if ($LASTEXITCODE -ne 0) { throw "独立路由器构建失败，退出码 $LASTEXITCODE" }

$built = Join-Path $targetDir 'release\starlink-dimension-router.exe'
if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "未找到构建产物: $built" }
$destination = Join-Path $releaseDir 'starlink-dimension-router.exe'
Copy-Item -LiteralPath $built -Destination $destination -Force
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
Set-Content -LiteralPath (Join-Path $releaseDir 'SHA256.txt') -Value "$hash  starlink-dimension-router.exe" -Encoding ascii
Write-Output "release=$destination"
Write-Output "sha256=$hash"
