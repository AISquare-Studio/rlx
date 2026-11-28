# fake-brain/spawns-child, for Windows: see spawns-child.
$null = [Console]::In.ReadLine()
$child = Start-Process -FilePath powershell -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 300'
Set-Content -Path (Join-Path $env:AISQUARE_HOME 'fake-brain-child.pid') -Value $child.Id
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
while ($null -ne [Console]::In.ReadLine()) { }
