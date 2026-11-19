@echo off
setlocal
cd /d "%~dp0" || exit /b 1
title Sculith
if not exist "sculith.exe" (
    echo Missing sculith.exe. Extract the complete ZIP first.
    exit /b 1
)
if /i "%~1"=="--check" goto check
sculith.exe run %*
set "SCULITH_EXIT=%ERRORLEVEL%"
if not "%SCULITH_EXIT%"=="0" (
    echo Server stopped with an error. See the message above.
    pause
)
exit /b %SCULITH_EXIT%

:check
sculith.exe %*
exit /b %ERRORLEVEL%
