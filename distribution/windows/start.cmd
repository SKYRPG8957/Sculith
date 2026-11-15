@echo off
setlocal
cd /d "%~dp0"
title Sculith - local shared-world preview
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
echo Sculith local shared-world preview
echo Add a Bedrock server: 127.0.0.1  port 19132
echo Add a Java 26.3 server: 127.0.0.1:25565
echo Both previews need explicit settings. Official client rendering is unverified.
echo Press Ctrl+C to save the world and stop.
echo.
sculith.exe run --config "sculith.toml"
set "SCULITH_EXIT=%ERRORLEVEL%"
if not "%SCULITH_EXIT%"=="0" echo Server stopped with an error. Keep the message above when reporting it.
pause
exit /b %SCULITH_EXIT%

:check
sculith.exe check --config "sculith.toml"
exit /b %ERRORLEVEL%
