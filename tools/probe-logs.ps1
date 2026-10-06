# 🔬 Critical experiment: can logsSubscribe carry instruction bytes for free?
#
# If "Program data:" logs contain the full base64 instruction data, then a
# WebSocket subscription is enough to decode launches in real time — no paid
# gRPC Yellowstone endpoint needed. If they are truncated or absent, the only
# free option is polling signatures.

$ErrorActionPreference = "Stop"
$key = $env:HELIUS_API_KEY
$ws = "wss://mainnet.helius-rpc.com/?api-key=$key"
$pump = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"

Write-Output "connecting..."
$socket = New-Object System.Net.WebSockets.ClientWebSocket
$cts = New-Object System.Threading.CancellationTokenSource
$cts.CancelAfter(25000)
$socket.ConnectAsync([Uri]$ws, $cts.Token).GetAwaiter().GetResult()
Write-Output "connected: $($socket.State)"

# `params` is a POSITIONAL array: [filter, commitment]. Writing
# `{...},"commitment":"confirmed"` leaves a dangling key inside the array and the
# server answers "Parse error" with no hint about which byte offended it.
# `params` is [filter, RpcTransactionLogsConfig]. A bare `"confirmed"` string
# makes the server reject the call with "expected struct RpcTransactionLogsConfig",
# which names the type but not the shape that would have worked.
$req = '{"jsonrpc":"2.0","id":1,"method":"logsSubscribe","params":[{"mentions":["' + $pump + '"]},{"commitment":"confirmed"}]}'
# A here-string keeps the newline right after `@"`, and a leading newline makes
# the server answer "Parse error" instead of "Invalid params". Trim it.
$req = $req.Trim()
$bytes = [Text.Encoding]::UTF8.GetBytes($req)
$seg = New-Object ArraySegment[byte] (,$bytes)
$socket.SendAsync($seg, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $cts.Token).GetAwaiter().GetResult()
Write-Output "subscribed, listening for 40s..."

$deadline = (Get-Date).AddSeconds(40)
$buf = New-Object byte[] 65536
$seen = 0
$pdCount = 0
$pdMaxLen = 0
$samples = @()

while ((Get-Date) -lt $deadline -and $seen -lt 12) {
    $ms = New-Object System.IO.MemoryStream
    do {
        $rbuf = New-Object byte[] 65536
        $rseg = New-Object ArraySegment[byte] (,$rbuf)
        $rcts = New-Object System.Threading.CancellationTokenSource
        $rcts.CancelAfter(8000)
        try {
            $res = $socket.ReceiveAsync($rseg, $rcts.Token).GetAwaiter().GetResult()
        } catch { break }
        if ($res.Count -gt 0) { $ms.Write($rbuf, 0, $res.Count) }
        if ($res.EndOfMessage) { break }
    } while ($true)

    $text = [Text.Encoding]::UTF8.GetString($ms.ToArray())
    if ($text.Length -eq 0) { continue }
    try { $msg = $text | ConvertFrom-Json } catch { continue }
    if (-not $msg.params) { Write-Output "  (non-notify: $($text.Substring(0,[Math]::Min(160,$text.Length))))"; continue }

    $seen++
    Write-Output ""
    Write-Output "=== RAW notification #$seen ($($text.Length) chars) ==="
    Write-Output $text
    $value = $msg.params.value
    if (-not $value) { Write-Output "  (params.value missing; keys: $(($msg.params.PSObject.Properties.Name) -join ', '))"; continue }
    $sig = $value.signature
    $logs = @($value.logs)
    $pd = @($logs | Where-Object { $_ -like "Program data:*" })
    $pdCount += $pd.Count
    foreach ($line in $pd) {
        $payload = $line.Substring(14)
        if ($payload.Length -gt $pdMaxLen) { $pdMaxLen = $payload.Length }
        if ($samples.Count -lt 6) { $samples += $payload }
    }
    Write-Output "--- logs=$($logs.Count)  program-data-lines=$($pd.Count)  err=$($value.err)"
    $logs | Select-Object -First 8 | ForEach-Object {
        $s = if ($_.Length -gt 100) { $_.Substring(0,100) + " ...[+$($_.Length-100)]" } else { $_ }
        Write-Output "      $s"
    }
    if ($seen -ge 2) { break }
}

Write-Output ""
Write-Output "================================================"
Write-Output "notifications seen      : $seen"
Write-Output "Program data: lines     : $pdCount"
Write-Output "longest payload (chars) : $pdMaxLen"
Write-Output "base64 of 8-byte disc   : ~12 chars; 154-byte ix = ~206 chars"
Write-Output ""
Write-Output "sample payloads:"
$samples | Select-Object -First 6 | ForEach-Object {
    $short = if ($_.Length -gt 110) { $_.Substring(0,110) + "..." } else { $_ }
    Write-Output "  len=$($_.Length)  $short"
}
$socket.Dispose()