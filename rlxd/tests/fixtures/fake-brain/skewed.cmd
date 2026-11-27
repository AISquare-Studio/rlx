@echo off
rem fake-brain/skewed, for Windows: see skewed and skewed.ps1. cmd reads at most
rem 1023 characters of a line, and rlxd's hello is longer: PowerShell reads it whole.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0skewed.ps1"
exit /b %ERRORLEVEL%
