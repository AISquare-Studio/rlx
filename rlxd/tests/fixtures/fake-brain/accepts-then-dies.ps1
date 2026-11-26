# fake-brain/accepts-then-dies, for Windows: see accepts-then-dies.
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"accepted","id":1}')
[Console]::Out.Flush()
exit 1
