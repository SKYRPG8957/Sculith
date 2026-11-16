@echo off
setlocal
cd /d "%~dp0"
title Sculith
if not exist "sculith.exe" (
    echo sculith.exe is missing. Extract the whole ZIP before starting.
    pause
    exit /b 1
)
if not exist "sculith.toml" (
    copy /b "sculith.local.example.toml" "sculith.toml" >nul
    if errorlevel 1 (
        echo Could not create sculith.toml. Move this folder to a writable location.
        pause
        exit /b 1
    )
)
if /i "%~1"=="--check" goto check
sculith.exe run --config "sculith.toml"
set "SCULITH_EXIT=%ERRORLEVEL%"
if not "%SCULITH_EXIT%"=="0" echo Server stopped with an error. Check the log above.
pause
exit /b %SCULITH_EXIT%

:check
sculith.exe check --config "sculith.toml"
exit /b %ERRORLEVEL%
