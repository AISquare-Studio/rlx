@echo off
rem fake-brain/accepts-unterminated, for Windows: see accepts-unterminated and
rem accepts-unterminated.ps1. cmd reads at most 1023 characters of a line, and rlxd's hello is
rem longer: PowerShell reads it whole.
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0accepts-unterminated.ps1"
exit /b %ERRORLEVEL%
