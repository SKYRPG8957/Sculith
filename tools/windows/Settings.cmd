@echo off
setlocal
cd /d "%~dp0" || exit /b 1
if not exist "sculith.toml" (
    sculith.exe --init-settings || exit /b 1
)
start "" notepad.exe "%~dp0sculith.toml"
