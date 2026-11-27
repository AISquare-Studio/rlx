@echo off
rem fake-brain/spawns-child, for Windows: see spawns-child and spawns-child.ps1.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0spawns-child.ps1"
exit /b %ERRORLEVEL%
