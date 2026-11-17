@echo off
setlocal
cd /d "%~dp0"
title Sculith
if not exist "sculith.exe" (
    echo Build sculith.exe first and place it beside this launcher.
    pause
    exit /b 1
)
if /i "%~1"=="--check" goto check
sculith.exe run %*
set "SCULITH_EXIT=%ERRORLEVEL%"
if not "%SCULITH_EXIT%"=="0" echo Server stopped with an error. Check the log above.
pause
exit /b %SCULITH_EXIT%

:check
sculith.exe check
exit /b %ERRORLEVEL%
