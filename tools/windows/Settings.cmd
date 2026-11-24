@echo off
setlocal
cd /d "%~dp0" || exit /b 1
if not exist "server.properties" (
    sculith.exe --init-server-properties || exit /b 1
)
start "" notepad.exe "%~dp0server.properties"
