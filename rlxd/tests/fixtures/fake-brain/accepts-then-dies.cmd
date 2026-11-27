@echo off
rem fake-brain/accepts-then-dies, for Windows: see accepts-then-dies and accepts-then-dies.ps1. cmd reads at most
rem 1023 characters of a line, and rlxd's hello is longer: PowerShell reads it whole.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0accepts-then-dies.ps1"
exit /b %ERRORLEVEL%
