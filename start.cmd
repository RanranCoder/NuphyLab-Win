@echo off
rem NuPhyLab Windows: start the local service and open the editor.
rem Requires the Air75 V3 connected by USB data cable, in wired mode, with
rem NuPhyIO and other lighting software closed.
cd /d "%~dp0"
if not exist node_modules\nul if not exist ui\dist\index.html (
  echo First run: building the editor UI...
  call npm --prefix ui ci || goto :failed
  call npm run build:ui || goto :failed
)
set AIR_STUDIO_PREFERRED_PORT=56980
start "NuPhyLab service" cmd /c "node server\main.mjs & pause"
timeout /t 2 /nobreak >nul
start "" http://127.0.0.1:56980/
echo NuPhyLab is starting at http://127.0.0.1:56980/
echo Close the service window to stop it.
exit /b 0
:failed
echo Build failed. Check that Node.js 22+ is installed.
pause
