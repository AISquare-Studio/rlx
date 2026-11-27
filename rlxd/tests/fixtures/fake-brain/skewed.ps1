# fake-brain/skewed, for Windows: see skewed.
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"type":"hello","protocol":99,"pid":' + $PID + ',"rlx_version":"9.9.9"}')
[Console]::Out.Flush()
while ($null -ne [Console]::In.ReadLine()) { }
