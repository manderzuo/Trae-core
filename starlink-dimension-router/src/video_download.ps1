$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$deliveryRequest = __REQUEST__
$deliveryUrl = __URL__
$deliveryWorkspace = __WORKSPACE__
$deliveryPart = $null
$deliveryHttp = $null
$deliveryInput = $null
$deliveryOutput = $null
try {
    if ([string]::IsNullOrWhiteSpace($deliveryWorkspace)) { $deliveryWorkspace = (Get-Location).ProviderPath }
    $deliveryWorkspace = [IO.Path]::GetFullPath($deliveryWorkspace)
    if (-not [IO.Directory]::Exists($deliveryWorkspace)) { [IO.Directory]::CreateDirectory($deliveryWorkspace) | Out-Null }
    $deliveryName = 'seedance-' + $deliveryRequest + '.mp4'
    $deliveryPath = [IO.Path]::Combine($deliveryWorkspace, $deliveryName)
    if ([IO.File]::Exists($deliveryPath)) {
        # A retry must never silently overwrite even an unrelated existing file.
        $deliveryPath = [IO.Path]::Combine($deliveryWorkspace, 'seedance-' + $deliveryRequest + '-' + [Guid]::NewGuid().ToString('N') + '.mp4')
    }
    $deliveryPart = $deliveryPath + '.' + [Guid]::NewGuid().ToString('N') + '.part'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $deliveryHttp = [Net.HttpWebRequest]::Create($deliveryUrl)
    $deliveryHttp.AllowAutoRedirect = $false
    $deliveryHttp.Timeout = 300000
    $deliveryHttp.ReadWriteTimeout = 300000
    $deliveryHttp.Method = 'GET'
    $deliveryResponse = $deliveryHttp.GetResponse()
    try {
        if ([int]$deliveryResponse.StatusCode -ne 200 -or $deliveryResponse.ContentType -notlike 'video/mp4*') { throw 'Invalid video response' }
        if ($deliveryResponse.ContentLength -gt 4294967296) { throw 'Video exceeds 4 GiB limit' }
        $deliveryInput = $deliveryResponse.GetResponseStream()
        $deliveryOutput = [IO.File]::Open($deliveryPart, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $deliveryBuffer = New-Object byte[] 65536
        $deliveryTotal = [long]0
        $deliveryTimer = [Diagnostics.Stopwatch]::StartNew()
        while (($deliveryCount = $deliveryInput.Read($deliveryBuffer, 0, $deliveryBuffer.Length)) -gt 0) {
            if ($deliveryTimer.Elapsed.TotalSeconds -gt 300) { throw 'Download deadline exceeded' }
            $deliveryTotal += $deliveryCount
            if ($deliveryTotal -gt 4294967296) { throw 'Video exceeds 4 GiB limit' }
            $deliveryOutput.Write($deliveryBuffer, 0, $deliveryCount)
        }
        $deliveryOutput.Dispose(); $deliveryOutput = $null
        $deliveryInput.Dispose(); $deliveryInput = $null
        if ($deliveryTotal -lt 12 -or ($deliveryResponse.ContentLength -ge 0 -and $deliveryTotal -ne $deliveryResponse.ContentLength)) { throw 'Incomplete video' }
    } finally { $deliveryResponse.Dispose() }
    $deliveryCheck = [IO.File]::OpenRead($deliveryPart)
    try {
        $deliveryHead = New-Object byte[] 12
        if ($deliveryCheck.Read($deliveryHead, 0, 12) -ne 12 -or [Text.Encoding]::ASCII.GetString($deliveryHead, 4, 4) -ne 'ftyp') { throw 'Invalid MP4 header' }
    } finally { $deliveryCheck.Dispose() }
    [IO.File]::Move($deliveryPart, $deliveryPath)
    $deliveryPart = $null
    $deliveryReceipt = @{seedance_delivery=1; request_id=$deliveryRequest; status='saved'; path=$deliveryPath; bytes=$deliveryTotal}
    Write-Output ('SEEDANCE_DELIVERY_RECEIPT=' + ($deliveryReceipt | ConvertTo-Json -Compress))
} catch {
    # Never echo the signed URL or a transport exception containing it.
    Write-Output ('SEEDANCE_DELIVERY_RECEIPT=' + (@{seedance_delivery=1; request_id=$deliveryRequest; status='failed'; error='download_failed'} | ConvertTo-Json -Compress))
    exit 1
} finally {
    if ($deliveryOutput) { $deliveryOutput.Dispose() }
    if ($deliveryInput) { $deliveryInput.Dispose() }
    if ($deliveryHttp) { $deliveryHttp.Abort() }
    if ($deliveryPart -and [IO.File]::Exists($deliveryPart)) { [IO.File]::Delete($deliveryPart) }
}
