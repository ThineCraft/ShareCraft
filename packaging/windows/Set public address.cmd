@echo off
rem Turns ShareCraft production mode on or off for the signed-in Windows user.
rem Run this copy from the installed program folder. See README.html.
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\public-address.ps1" %*
set "SHARECRAFT_PUBLIC_ADDRESS_EXIT=%ERRORLEVEL%"
echo.
pause
exit /b %SHARECRAFT_PUBLIC_ADDRESS_EXIT%
