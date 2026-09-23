param(
  [string]$ReleaseRoot = '',
  [string]$DataDir = 'D:\gpt\starlink-dimension-router-data',
  [string]$EnvironmentFile = 'D:\gpt\starlink-core-secrets\key-encryption.env',
  [switch]$Background
)

$ErrorActionPreference = 'Stop'
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ([string]::IsNullOrWhiteSpace($ReleaseRoot)) {
  $ReleaseRoot = if (Test-Path -LiteralPath (Join-Path $scriptRoot 'release') -PathType Container) {
    $scriptRoot
  } else {
    'D:\gpt\starlink-core-persist-release'
  }
}
$exe = Join-Path $ReleaseRoot 'release\starlink-dimension-router.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
  throw "未找到 Core 程序: $exe"
}
if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
  New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
}

$environmentPath = [System.IO.Path]::GetFullPath($EnvironmentFile)
$dataPath = [System.IO.Path]::GetFullPath($DataDir).TrimEnd([System.IO.Path]::DirectorySeparatorChar)
if ($environmentPath.StartsWith($dataPath + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
  throw 'Core Key 加密环境文件必须位于 Core 数据目录之外。'
}
if (-not (Test-Path -LiteralPath $environmentPath -PathType Leaf)) {
  throw "Core Key 加密环境文件不存在：$environmentPath；请先运行密钥初始化脚本。"
}

$allowedNames = @(
  'STARLINK_ROUTER_KEY_ENCRYPTION_KEY',
  'STARLINK_ROUTER_KEY_ENCRYPTION_KEY_VERSION',
  'STARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS'
)
$settings = @{}
foreach ($line in [System.IO.File]::ReadAllLines($environmentPath)) {
  if ([string]::IsNullOrWhiteSpace($line) -or $line.TrimStart().StartsWith('#')) { continue }
  if ($line -notmatch '^([A-Z0-9_]+)=(.*)$' -or $allowedNames -notcontains $Matches[1] -or $settings.ContainsKey($Matches[1])) {
    throw 'Core Key 加密环境文件格式无效。'
  }
  $settings[$Matches[1]] = $Matches[2]
}
if (-not $settings.ContainsKey($allowedNames[0])) {
  throw 'Core Key 加密环境文件缺少活动密钥。'
}
$activeKeyBytes = $null
try {
  $activeKeyBytes = [Convert]::FromBase64String($settings[$allowedNames[0]])
  if ($activeKeyBytes.Length -ne 32) { throw 'invalid length' }
} catch {
  throw 'Core Key 加密环境文件中的活动密钥无效。'
} finally {
  if ($null -ne $activeKeyBytes) { [Array]::Clear($activeKeyBytes, 0, $activeKeyBytes.Length) }
}
$keyVersion = 1
if ($settings.ContainsKey($allowedNames[1]) -and
    (-not [uint32]::TryParse($settings[$allowedNames[1]], [ref]$keyVersion) -or $keyVersion -eq 0)) {
  throw 'Core Key 加密密钥版本无效。'
}
$previousKeys = if ($settings.ContainsKey($allowedNames[2])) { $settings[$allowedNames[2]] } else { '{}' }
try {
  $previous = ConvertFrom-Json -InputObject $previousKeys -ErrorAction Stop
  if ($previous -isnot [System.Management.Automation.PSCustomObject]) { throw 'not an object' }
  foreach ($property in $previous.PSObject.Properties) {
    $previousVersion = 0
    $previousKeyBytes = $null
    if (-not [uint32]::TryParse($property.Name, [ref]$previousVersion) -or $previousVersion -eq 0 -or $previousVersion -eq $keyVersion) {
      throw 'invalid key version'
    }
    try {
      $previousKeyBytes = [Convert]::FromBase64String([string]$property.Value)
      if ($previousKeyBytes.Length -ne 32) { throw 'invalid key length' }
    } finally {
      if ($null -ne $previousKeyBytes) { [Array]::Clear($previousKeyBytes, 0, $previousKeyBytes.Length) }
    }
  }
} catch {
  throw 'Core Key 加密环境文件中的旧版本密钥配置无效。'
}

$env:STARLINK_ROUTER_KEY_ENCRYPTION_KEY = $settings[$allowedNames[0]]
$env:STARLINK_ROUTER_KEY_ENCRYPTION_KEY_VERSION = [string]$keyVersion
$env:STARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS = $previousKeys

# 公网地址保存在 router.json；这里仅固定数据目录，避免依赖当前终端会话。
$env:STARLINK_ROUTER_DATA_DIR = $DataDir
Remove-Item Env:STARLINK_ROUTER_PUBLIC_BASE_URL -ErrorAction SilentlyContinue

if ($Background) {
  Start-Process -FilePath $exe -WorkingDirectory (Split-Path -Parent $exe) -WindowStyle Hidden
  exit 0
}

& $exe
exit $LASTEXITCODE
