@echo off
call "%~dp0start.cmd" --offline %*
exit /b %ERRORLEVEL%
