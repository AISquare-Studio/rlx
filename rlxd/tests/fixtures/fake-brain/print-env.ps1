# fake-brain/print-env, for Windows: see print-env.
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
$busy = Join-Path $env:AISQUARE_HOME 'fake-brain-busy'
$hold = Join-Path $env:AISQUARE_HOME 'fake-brain-hold'
while ($null -ne ($request = [Console]::In.ReadLine())) {
    $id = [int64]($request | ConvertFrom-Json).id
    [Console]::Out.WriteLine('{"type":"accepted","id":' + $id + '}')
    [Console]::Out.Flush()
    Set-Content -Path $busy -Value $id
    while (Test-Path $hold) { Start-Sleep -Milliseconds 100 }
    $result = @{ pid = $PID; CLAUDE_CONFIG_DIR = "$env:CLAUDE_CONFIG_DIR" }
    $reply = @{ type = 'reply'; id = $id; ok = $true; result = $result }
    [Console]::Out.WriteLine(($reply | ConvertTo-Json -Compress))
    [Console]::Out.Flush()
}
