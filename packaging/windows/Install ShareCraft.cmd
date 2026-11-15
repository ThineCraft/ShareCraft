@echo off
rem Installs ShareCraft for the signed-in Windows user, without administrator rights.
rem Double-click it, or run it from a Command Prompt; extra options (such as -Reinstall)
rem are passed on to scripts\install.ps1. See README.html.
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\install.ps1" %*
set "SHARECRAFT_INSTALL_EXIT=%ERRORLEVEL%"
echo.
pause
exit /b %SHARECRAFT_INSTALL_EXIT%
