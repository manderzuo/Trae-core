$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$cfg = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__CONFIG_BASE64__')) | ConvertFrom-Json
$client = $null
try {
    Add-Type -AssemblyName System.Net.Http
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $handler = New-Object Net.Http.HttpClientHandler
    $handler.AllowAutoRedirect = $false
    $client = New-Object Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(120)
    $client.DefaultRequestHeaders.Add('X-Seedance-Upload', [string]$cfg.authorization)
    for ($i = 0; $i -lt $cfg.paths.Count; $i++) {
        $file = Get-Item -LiteralPath ([string]$cfg.paths[$i]) -Force
        if ($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $file.Length -lt 8 -or $file.Length -gt 33554432) { throw 'invalid image file' }
        $stream = [IO.File]::OpenRead($file.FullName)
        $content = New-Object Net.Http.StreamContent($stream)
        $reply = $null
        try {
            $content.Headers.ContentType = New-Object Net.Http.Headers.MediaTypeHeaderValue('application/octet-stream')
            $reply = $client.PostAsync(([string]$cfg.base + '/' + $i), $content).GetAwaiter().GetResult()
            if (-not $reply.IsSuccessStatusCode) { throw 'image upload rejected' }
        } finally {
            if ($null -ne $reply) { $reply.Dispose() }
            $content.Dispose()
            $stream.Dispose()
        }
    }
    Write-Output ('SEEDANCE_REFERENCE_UPLOAD=' + (@{ id=$cfg.id; status='uploaded'; files=$cfg.paths.Count } | ConvertTo-Json -Compress))
} catch {
    Write-Output 'SEEDANCE_REFERENCE_UPLOAD_FAILED: image unavailable, network denied, or upload rejected. No video was submitted.'
    exit 1
} finally {
    if ($null -ne $client) { $client.Dispose() }
}
