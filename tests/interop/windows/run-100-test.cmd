@echo off
setlocal
cd /d "%~dp0" || exit /b 1
chcp 65001 >nul
".\runtime\python.exe" -X utf8 ".\run-test.py" %*
set "SCULITH_RESULT=%ERRORLEVEL%"
pause
exit /b %SCULITH_RESULT%
