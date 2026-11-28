# fake-brain/print-env, for Windows: see print-env. Like it, it says `accepted` before it
# does anything else, and parses no JSON: a first ConvertFrom-Json can take PowerShell
# longer on a cold, loaded runner than rlxd waits for `accepted` (2 s).
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
$busy = Join-Path $env:AISQUARE_HOME 'fake-brain-busy'
$hold = Join-Path $env:AISQUARE_HOME 'fake-brain-hold'
# JSON-escaped, as print-env's sed does: backslashes, then quotes.
$dir = "$env:CLAUDE_CONFIG_DIR".Replace('\', '\\').Replace('"', '\"')
while ($null -ne ($request = [Console]::In.ReadLine())) {
    $id = [regex]::Match($request, '"id":([0-9]+)').Groups[1].Value
    [Console]::Out.WriteLine('{"type":"accepted","id":' + $id + '}')
    [Console]::Out.Flush()
    Set-Content -Path $busy -Value $id
    while (Test-Path $hold) { Start-Sleep -Milliseconds 100 }
    [Console]::Out.WriteLine('{"type":"reply","id":' + $id + ',"ok":true,"result":{"pid":' + $PID + ',"CLAUDE_CONFIG_DIR":"' + $dir + '"}}')
    [Console]::Out.Flush()
}
