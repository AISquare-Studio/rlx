@echo off
rem fake-claude/auth-status, for Windows: see auth-status.
if "%~1"=="--version" if "%~2"=="" goto version
if "%~1"=="auth" if "%~2"=="status" if "%~3"=="--json" if "%~4"=="" goto status
echo fake-claude/auth-status: no reply recorded for: %* 1>&2
exit /b 2
:version
echo 2.1.314 (Claude Code)
exit /b 0
:status
echo {"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "email": "probe@example.com", "orgId": "00000000-0000-4000-8000-000000000000", "orgName": "Probe Org", "subscriptionType": "pro"}
exit /b 0
