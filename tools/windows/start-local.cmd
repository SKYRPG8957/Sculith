@echo off
if /i "%~1"=="--check" (
    call "%~dp0start.cmd" --check --offline
) else (
    call "%~dp0start.cmd" --offline %*
)
exit /b %ERRORLEVEL%
