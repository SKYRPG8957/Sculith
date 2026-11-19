@echo off
setlocal
set "RUST_LOG=info,pumpkin_core::net::bedrock=debug"
call "%~dp0start.cmd" %*
exit /b %ERRORLEVEL%
