param(
  [Parameter(Mandatory=$true)][string]$SourceRoot,
  [string]$TargetRoot = '',
  [Parameter(Mandatory=$true)][string]$MigrationId,
  [switch]$Apply
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($TargetRoot)) { $TargetRoot = Join-Path $repoRoot 'data' }
if ([string]::IsNullOrWhiteSpace($MigrationId)) { throw 'MigrationId 不能为空' }
if (-not (Test-Path -LiteralPath $SourceRoot -PathType Container)) { throw "源目录不存在: $SourceRoot" }
$names = @('data\core.sqlite3','data\api_keys.json','data\remaining_credits.json','data\video_tasks.json')
$hashes = [ordered]@{}
foreach ($name in $names) {
  $path = Join-Path $SourceRoot $name
  if (Test-Path -LiteralPath $path -PathType Leaf) { $hashes[$name] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
}
$report = [ordered]@{
  migration_id = $MigrationId
  source_root = $SourceRoot
  target_root = $TargetRoot
  file_count = $hashes.Count
  source_hashes = $hashes
  unmapped_records = @('legacy account-pool rows are not imported automatically')
  applied = $false
}
if ($Apply) {
  $targetDb = Join-Path $TargetRoot 'data\core.sqlite3'
  if (Test-Path -LiteralPath $targetDb -PathType Leaf) { throw '目标 Core 已有数据库，拒绝覆盖' }
  $sourceDb = Join-Path $SourceRoot 'data\core.sqlite3'
  New-Item -ItemType Directory -Force -Path (Join-Path $TargetRoot 'data') | Out-Null
  if (Test-Path -LiteralPath $sourceDb -PathType Leaf) { Copy-Item -LiteralPath $sourceDb -Destination $targetDb -Force }
  $report.applied = $true
}
$report | ConvertTo-Json -Depth 8
