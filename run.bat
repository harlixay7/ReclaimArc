@echo off
rem SpaceExtract launcher — builds (if needed) and starts the desktop app.
setlocal
cd /d "%~dp0"

where cargo >nul 2>nul
if errorlevel 1 (
  echo cargo not found. Install Rust: https://rustup.rs
  exit /b 1
)

if not exist "apps\desktop\node_modules" (
  echo Installing frontend dependencies...
  pushd apps\desktop
  call npm install
  if errorlevel 1 ( echo npm install failed & exit /b 1 )
  popd
)

echo Building SpaceExtract (first build takes a while)...
pushd apps\desktop
call npm run tauri build
if errorlevel 1 ( echo build failed & exit /b 1 )
popd

echo Launching SpaceExtract...
start "" "target\release\spacextract-desktop.exe"
endlocal