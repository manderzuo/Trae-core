param(
  [ValidateRange(1, 4294967295)]
  [uint32]$Version = 1,
  [string]$EnvironmentFile = '',
  [string]$DataDir = '',
  [string]$ServiceAccount = ''
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($EnvironmentFile)) { $EnvironmentFile = Join-Path $repoRoot 'secrets\key-encryption.env' }
if ([string]::IsNullOrWhiteSpace($DataDir)) { $DataDir = Join-Path $repoRoot 'data' }
$environmentPath = [System.IO.Path]::GetFullPath($EnvironmentFile)
$dataPath = [System.IO.Path]::GetFullPath($DataDir).TrimEnd([System.IO.Path]::DirectorySeparatorChar)
if ($environmentPath.StartsWith($dataPath + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase)) {
  throw '加密密钥文件必须放在 Core 数据目录之外。'
}
if (Test-Path -LiteralPath $environmentPath) {
  throw "密钥文件已存在，拒绝覆盖：$environmentPath"
}

$secretDirectory = [System.IO.Path]::GetDirectoryName($environmentPath)
[System.IO.Directory]::CreateDirectory($secretDirectory) | Out-Null

$currentIdentity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$serviceIdentity = if ([string]::IsNullOrWhiteSpace($ServiceAccount)) {
  $currentIdentity.User
} else {
  (New-Object System.Security.Principal.NTAccount($ServiceAccount)).Translate([System.Security.Principal.SecurityIdentifier])
}
$systemIdentity = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-18')
$inheritance = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit

$directoryAcl = New-Object System.Security.AccessControl.DirectorySecurity
$directoryAcl.SetAccessRuleProtection($true, $false)
$directoryAcl.SetOwner($currentIdentity.User)
$directoryAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($currentIdentity.User, [System.Security.AccessControl.FileSystemRights]::Modify, $inheritance, [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow)))
$directoryAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($serviceIdentity, [System.Security.AccessControl.FileSystemRights]::ReadAndExecute, $inheritance, [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow)))
$directoryAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($systemIdentity, [System.Security.AccessControl.FileSystemRights]::FullControl, $inheritance, [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow)))
Set-Acl -LiteralPath $secretDirectory -AclObject $directoryAcl

$keyBytes = New-Object byte[] 32
$randomGenerator = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$randomGenerator.GetBytes($keyBytes)
$randomGenerator.Dispose()
$encodedKey = [Convert]::ToBase64String($keyBytes)
$lines = @(
  "STARLINK_ROUTER_KEY_ENCRYPTION_KEY=$encodedKey",
  "STARLINK_ROUTER_KEY_ENCRYPTION_KEY_VERSION=$Version",
  'STARLINK_ROUTER_KEY_ENCRYPTION_PREVIOUS_KEYS={}'
)
$encoding = New-Object System.Text.UTF8Encoding($false)
$stream = New-Object System.IO.FileStream($environmentPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
try {
  $writer = New-Object System.IO.StreamWriter($stream, $encoding)
  foreach ($line in $lines) { $writer.WriteLine($line) }
  $writer.Flush()
  $writer.Dispose()
} finally {
  $stream.Dispose()
  [Array]::Clear($keyBytes, 0, $keyBytes.Length)
}

$fileAcl = New-Object System.Security.AccessControl.FileSecurity
$fileAcl.SetAccessRuleProtection($true, $false)
$fileAcl.SetOwner($currentIdentity.User)
$fileAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($currentIdentity.User, [System.Security.AccessControl.FileSystemRights]::Read -bor [System.Security.AccessControl.FileSystemRights]::Write, [System.Security.AccessControl.AccessControlType]::Allow)))
$fileAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($serviceIdentity, [System.Security.AccessControl.FileSystemRights]::Read, [System.Security.AccessControl.AccessControlType]::Allow)))
$fileAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($systemIdentity, [System.Security.AccessControl.FileSystemRights]::FullControl, [System.Security.AccessControl.AccessControlType]::Allow)))
Set-Acl -LiteralPath $environmentPath -AclObject $fileAcl
$encodedKey = $null
$lines = $null

Write-Output "已创建受限权限的 Core Key 加密环境文件：$environmentPath。密钥内容未显示；请使用同一服务账户启动 Core。"
