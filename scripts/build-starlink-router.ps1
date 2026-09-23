param(
  [string]$OutputRoot = '',
  [string]$CargoTargetDir = '',
  [string]$CargoCommand = 'cargo'
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = [System.IO.Path]::GetFullPath($repoRoot)
if ([string]::IsNullOrWhiteSpace($OutputRoot)) { $OutputRoot = Join-Path $repoRoot 'dist' }
if ([string]::IsNullOrWhiteSpace($CargoTargetDir)) { $CargoTargetDir = Join-Path $repoRoot 'target' }
$OutputRoot = [System.IO.Path]::GetFullPath($OutputRoot)
$targetDir = [System.IO.Path]::GetFullPath($CargoTargetDir)
$env:CARGO_TARGET_DIR = $targetDir
$manifest = Join-Path $repoRoot 'starlink-dimension-router\Cargo.toml'
$cargo = Get-Command -Name $CargoCommand -ErrorAction SilentlyContinue
if ($null -eq $cargo) { throw "找不到 Cargo 命令：$CargoCommand；请安装 Rust 或通过 -CargoCommand 指定可执行文件。" }
$releaseDir = Join-Path $OutputRoot 'release'

New-Item -ItemType Directory -Force -Path $OutputRoot, $releaseDir, $targetDir | Out-Null
& $cargo.Source build --release --manifest-path $manifest --offline
if ($LASTEXITCODE -ne 0) { throw "独立路由器构建失败，退出码 $LASTEXITCODE" }

$built = Join-Path $targetDir 'release\starlink-dimension-router.exe'
if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { throw "未找到构建产物: $built" }
$destination = Join-Path $releaseDir 'starlink-dimension-router.exe'
Copy-Item -LiteralPath $built -Destination $destination -Force
$hash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
Set-Content -LiteralPath (Join-Path $releaseDir 'SHA256.txt') -Value "$hash  starlink-dimension-router.exe" -Encoding ascii
Write-Output "release=$destination"
Write-Output "sha256=$hash"
