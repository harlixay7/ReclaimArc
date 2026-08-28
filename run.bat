@echo off
setlocal enabledelayedexpansion

rem =========================================================================
rem ReclaimArc -- Interactive Application Launcher and Development Console
rem =========================================================================

cd /d "%~dp0"

where cargo.exe >nul 2>nul
if errorlevel 1 (
    echo [!] Cargo / Rust toolchain not detected in PATH.
    echo [*] Launching automated dependency setup...
    call setup.bat
    if errorlevel 1 (
        echo [!] Setup failed. Please inspect missing dependencies.
        pause
        exit /b 1
    )
)

if "%1"=="--cli" goto launch_cli
if "%1"=="--test" goto run_tests
if "%1"=="--build" goto build_all
if "%1"=="--publish" goto publish_public
if "%1"=="--setup" goto run_setup
if "%1"=="--help" goto show_help

rem Default behavior: Launch Desktop GUI
:launch_gui
if exist "target\release\reclaimarc-desktop.exe" (
    echo [*] Starting ReclaimArc Desktop App...
    start "" "target\release\reclaimarc-desktop.exe"
    exit /b 0
) else (
    echo [*] Release binary not found. Building ReclaimArc Desktop App...
    pushd apps\desktop
    if not exist "node_modules" call npm install
    call npm run tauri build
    popd
    if exist "target\release\reclaimarc-desktop.exe" (
        echo [OK] Build complete. Starting ReclaimArc Desktop...
        start "" "target\release\reclaimarc-desktop.exe"
        exit /b 0
    ) else (
        echo [!] Build failed. Run "setup.bat" to verify all compiler requirements.
        pause
        exit /b 1
    )
)

:launch_cli
if not exist "target\release\reclaimarc.exe" (
    echo [*] Building release CLI binary...
    cargo build --release -p reclaimarc-cli
)
echo.
echo +=======================================================================+
echo ^|                     RECLAIMARC COMMAND-LINE INTERFACE                 ^|
echo +=======================================================================+
echo.
echo Usage examples:
echo   target\release\reclaimarc.exe inspect ^<archive^>
echo   target\release\reclaimarc.exe plan ^<archive^> ^<destination^>
echo   target\release\reclaimarc.exe extract ^<archive^> ^<destination^> --low-space --yes
echo.
cmd /k "target\release\reclaimarc.exe --help"
exit /b 0

:run_tests
echo [*] Executing full workspace test suite...
cargo test --workspace
exit /b %errorlevel%

:build_all
echo [*] Compiling release binaries and packaging desktop installer...
cargo build --release -p reclaimarc-cli
pushd apps\desktop
if not exist "node_modules" call npm install
call npm run tauri build
popd
echo.
echo [OK] All release artifacts generated in "target\release\":
echo      - CLI Binary: target\release\reclaimarc.exe
echo      - Desktop App: target\release\reclaimarc-desktop.exe
echo      - Setup Installer: target\release\bundle\nsis\ReclaimArc_0.2.0_x64-setup.exe
exit /b 0

:publish_public
echo [*] Running full workspace verification suite before publishing...
cargo test --workspace
if errorlevel 1 (
    echo [!] Tests failed. Aborting publish.
    exit /b 1
)
echo [*] Pushing verified main branch and release tags to Public repository...
git push public main --tags
if errorlevel 1 (
    echo [!] Failed to push to public repository. Please inspect remote configuration.
    exit /b 1
)
echo [OK] Successfully published to Public repository (https://github.com/harlixay7/ReclaimArc).
exit /b 0

:run_setup
call setup.bat
exit /b 0

:show_help
echo.
echo ReclaimArc Launcher Commands:
echo   run.bat          -- Launch the Desktop GUI Application [default]
echo   run.bat --cli    -- Open the CLI interactive console
echo   run.bat --test   -- Run the automated verification test suite
echo   run.bat --build  -- Build release binaries and installer packages
echo   run.bat --publish-- Run tests and publish release to public repository
echo   run.bat --setup  -- Run universal dependency bootstrap
echo   run.bat --help   -- Display this reference manual
echo.
exit /b 0
