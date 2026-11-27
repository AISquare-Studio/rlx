@echo off
rem fake-brain/print-env, for Windows: see print-env and print-env.ps1. cmd reads at most
rem 1023 characters of a line, and rlxd's hello is longer: PowerShell reads it whole.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0print-env.ps1"
exit /b %ERRORLEVEL%
