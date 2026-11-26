@echo off
rem fake-brain/mute, for Windows: see mute.
set /p HELLO=
echo {"type":"hello","protocol":1,"pid":1,"rlx_version":"0.0.0"}
more > nul
