@echo off
setlocal
set "RUST_LOG=info,sculith::protocol::bedrock=debug,raknet::server=debug,raknet::session=debug"
call "%~dp0start.cmd" %*
exit /b %ERRORLEVEL%
