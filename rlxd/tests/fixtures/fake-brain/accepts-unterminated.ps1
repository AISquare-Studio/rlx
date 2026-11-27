# fake-brain/accepts-unterminated, for Windows: see accepts-unterminated.
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
$request = [Console]::In.ReadLine()
$id = [regex]::Match($request, '"id":(\d+)').Groups[1].Value
[Console]::Out.Write('{"type":"accepted","id":' + $id + '}')
[Console]::Out.Flush()
Start-Sleep -Seconds 60
