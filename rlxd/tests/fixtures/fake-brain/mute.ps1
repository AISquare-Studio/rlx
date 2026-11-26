# fake-brain/mute, for Windows: see mute.
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":1,"pid":' + $PID + ',"rlx_version":"0.0.0"}')
[Console]::Out.Flush()
while ($null -ne [Console]::In.ReadLine()) { }
