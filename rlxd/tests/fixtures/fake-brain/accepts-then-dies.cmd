@echo off
rem fake-brain/accepts-then-dies, for Windows: see accepts-then-dies.
set /p HELLO=
echo {"type":"hello","protocol":1,"pid":1,"rlx_version":"0.0.0"}
set /p REQUEST=
echo {"type":"accepted","id":1}
exit /b 1
