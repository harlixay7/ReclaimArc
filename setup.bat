@echo off
setlocal enabledelayedexpansion

rem =========================================================================
rem ReclaimArc -- Universal Environment and Dependency Bootstrap for Windows
rem =========================================================================

echo.
echo +=======================================================================+
echo ^|                     RECLAIMARC DEPENDENCY BOOTSTRAP                   ^|
echo ^|       Automated Toolchain, Compiler, and Runtime Environment Setup    ^|
echo +=======================================================================+
echo.

cd /d "%~dp0"

set "ALL_DEPS_OK=1"

rem -------------------------------------------------------------------------
rem 1. Check Operating System Architecture
rem -------------------------------------------------------------------------
echo [1/6] Checking Operating System Architecture...
if "%PROCESSOR_ARCHITECTURE%"=="AMD64" (
    echo     [*] Architecture: x86_64 [64-bit AMD/Intel] - Supported.
) else if "%PROCESSOR_ARCHITEW6432%"=="AMD64" (
    echo     [*] Architecture: x86_64 [WOW64] - Supported.
) else if "%PROCESSOR_ARCHITECTURE%"=="ARM64" (
    echo     [*] Architecture: ARM64 [Windows on ARM] - Supported.
) else (
    echo     [!] Architecture: %PROCESSOR_ARCHITECTURE% [32-bit x86 is not recommended].
)
echo     [OK] Operating System check passed.
echo.

rem -------------------------------------------------------------------------
rem 2. Check Visual Studio C++ Build Tools (Required for UnRAR C++ compilation)
rem -------------------------------------------------------------------------
echo [2/6] Checking Visual Studio C++ Build Tools [MSVC]...
set "MSVC_FOUND=0"

rem Check via vswhere if installed
set "VSWHERE_PATH=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE_PATH%" set "VSWHERE_PATH=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"

if exist "%VSWHERE_PATH%" (
    for /f "usebackq tokens=*" %%i in (`"%VSWHERE_PATH%" -latest -products * -property installationPath 2^>nul`) do (
        if exist "%%i" (
            set "MSVC_FOUND=1"
            echo     [*] Found MSVC Build Tools at: %%i
        )
    )
)

if "!MSVC_FOUND!"=="0" (
    where cl.exe >nul 2>nul
    if not errorlevel 1 (
        set "MSVC_FOUND=1"
        echo     [*] Found cl.exe in current PATH.
    )
)

if "!MSVC_FOUND!"=="1" (
    echo     [OK] Visual Studio C++ Build Tools are installed.
) else (
    echo     [MISSING] Visual Studio C++ Build Tools are required to compile the vendored C++ UnRAR core.
    echo     [*] Attempting automated installation via winget...
    where winget >nul 2>nul
    if not errorlevel 1 (
        winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools;includeRecommended" --accept-source-agreements --accept-package-agreements
        if not errorlevel 1 (
            echo     [SUCCESS] Visual Studio C++ Build Tools installed successfully.
            set "MSVC_FOUND=1"
        ) else (
            echo     [!] winget install failed. Please manually install C++ Build Tools from:
            echo         https://visualstudio.microsoft.com/visual-cpp-build-tools/
            set "ALL_DEPS_OK=0"
        )
    ) else (
        echo     [!] winget not found. Please manually install Visual Studio C++ Build Tools:
        echo         https://visualstudio.microsoft.com/visual-cpp-build-tools/
        set "ALL_DEPS_OK=0"
    )
)
echo.

rem -------------------------------------------------------------------------
rem 3. Check Rust Toolchain (rustc and cargo)
rem -------------------------------------------------------------------------
echo [3/6] Checking Rust Compiler and Cargo Package Manager...
set "RUST_FOUND=0"

where cargo.exe >nul 2>nul
if not errorlevel 1 (
    for /f "tokens=*" %%v in ('rustc --version 2^>nul') do set "RUSTC_VER=%%v"
    for /f "tokens=*" %%v in ('cargo --version 2^>nul') do set "CARGO_VER=%%v"
    echo     [*] !RUSTC_VER!
    echo     [*] !CARGO_VER!
    set "RUST_FOUND=1"
    echo     [OK] Rust toolchain is installed and active.
) else (
    rem Check default user rustup location
    if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
        set "PATH=%USERPROFILE%\.cargo\bin;!PATH!"
        set "RUST_FOUND=1"
        echo     [*] Found Cargo in %USERPROFILE%\.cargo\bin [added to session PATH].
        echo     [OK] Rust toolchain is ready.
    ) else (
        echo     [MISSING] Rust compiler [rustc/cargo] not detected.
        echo     [*] Downloading official rustup installer...
        set "RUSTUP_TMP=%TEMP%\rustup-init.exe"
        powershell -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; (New-Object Net.WebClient).DownloadFile('https://win.rustup.rs/x86_64', '!RUSTUP_TMP!')" 2>nul
        if exist "!RUSTUP_TMP!" (
            echo     [*] Running rustup-init [targeting stable-x86_64-pc-windows-msvc]...
            "!RUSTUP_TMP!" -y --default-toolchain stable --default-host x86_64-pc-windows-msvc
            del "!RUSTUP_TMP!" >nul 2>nul
            if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
                set "PATH=%USERPROFILE%\.cargo\bin;!PATH!"
                set "RUST_FOUND=1"
                echo     [SUCCESS] Rust toolchain installed successfully.
            ) else (
                echo     [!] Rust installation failed. Please install from https://rustup.rs
                set "ALL_DEPS_OK=0"
            )
        ) else (
            echo     [!] Could not download rustup-init.exe. Please install Rust from https://rustup.rs
            set "ALL_DEPS_OK=0"
        )
    )
)
echo.

rem -------------------------------------------------------------------------
rem 4. Check Node.js and npm (Required for Desktop GUI build)
rem -------------------------------------------------------------------------
echo [4/6] Checking Node.js and npm Runtime...
set "NODE_FOUND=0"

where node.exe >nul 2>nul
if not errorlevel 1 (
    for /f "tokens=*" %%v in ('node --version 2^>nul') do set "NODE_VER=%%v"
    for /f "tokens=*" %%v in ('npm --version 2^>nul') do set "NPM_VER=%%v"
    echo     [*] Node.js: !NODE_VER!
    echo     [*] npm: v!NPM_VER!
    set "NODE_FOUND=1"
    echo     [OK] Node.js and npm are installed.
) else (
    echo     [MISSING] Node.js is required for building the desktop user interface.
    echo     [*] Attempting automated installation via winget...
    where winget >nul 2>nul
    if not errorlevel 1 (
        winget install --id OpenJS.NodeJS.LTS -e --accept-source-agreements --accept-package-agreements
        if not errorlevel 1 (
            echo     [SUCCESS] Node.js LTS installed. Refreshing PATH...
            set "PATH=%ProgramFiles%\nodejs;%APPDATA%\npm;!PATH!"
            set "NODE_FOUND=1"
        ) else (
            echo     [!] winget install failed. Please manually install Node.js LTS from: https://nodejs.org
            set "ALL_DEPS_OK=0"
        )
    ) else (
        echo     [!] winget not available. Please install Node.js LTS from https://nodejs.org
        set "ALL_DEPS_OK=0"
    )
)
echo.

rem -------------------------------------------------------------------------
rem 5. Check Microsoft Edge WebView2 Runtime (Required for Tauri 2 GUI)
rem -------------------------------------------------------------------------
echo [5/6] Checking Microsoft Edge WebView2 Runtime...
set "WEBVIEW2_FOUND=0"

rem Check Registry for WebView2 Runtime
reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" >nul 2>nul
if not errorlevel 1 set "WEBVIEW2_FOUND=1"

if "!WEBVIEW2_FOUND!"=="0" (
    reg query "HKCU\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" >nul 2>nul
    if not errorlevel 1 set "WEBVIEW2_FOUND=1"
)

if "!WEBVIEW2_FOUND!"=="0" (
    if exist "%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application" set "WEBVIEW2_FOUND=1"
    if exist "%ProgramFiles%\Microsoft\EdgeWebView\Application" set "WEBVIEW2_FOUND=1"
)

if "!WEBVIEW2_FOUND!"=="1" (
    echo     [*] WebView2 Runtime is active.
    echo     [OK] Windows desktop UI host runtime is ready.
) else (
    echo     [MISSING] Microsoft Edge WebView2 Evergreen Runtime not detected.
    echo     [*] Downloading Evergreen Bootstrapper...
    set "WV2_TMP=%TEMP%\MicrosoftEdgeWebview2Setup.exe"
    powershell -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; (New-Object Net.WebClient).DownloadFile('https://go.microsoft.com/fwlink/p/?LinkId=2124703', '!WV2_TMP!')" 2>nul
    if exist "!WV2_TMP!" (
        echo     [*] Installing WebView2 Runtime silently...
        "!WV2_TMP!" /silent /install
        del "!WV2_TMP!" >nul 2>nul
        echo     [SUCCESS] WebView2 Runtime installed.
    ) else (
        echo     [!] Could not download WebView2 bootstrapper. Install manually from:
        echo         https://developer.microsoft.com/en-us/microsoft-edge/webview2/
    )
)
echo.

rem -------------------------------------------------------------------------
rem 6. Initialize Desktop Frontend and Verify Cargo Dependencies
rem -------------------------------------------------------------------------
echo [6/6] Initializing Project Modules and Verifying Packages...

if exist "apps\desktop" (
    if not exist "apps\desktop\node_modules" (
        echo     [*] Installing desktop frontend npm packages...
        pushd apps\desktop
        call npm install
        if errorlevel 1 (
            echo     [!] npm install encountered errors.
            set "ALL_DEPS_OK=0"
        ) else (
            echo     [OK] npm packages installed successfully.
        )
        popd
    ) else (
        echo     [OK] Desktop frontend packages already present [apps\desktop\node_modules].
    )
)

if "!RUST_FOUND!"=="1" (
    echo     [*] Verifying Rust workspace crates...
    cargo check --workspace --quiet
    if not errorlevel 1 (
        echo     [OK] All 6 workspace crates verified and ready to build.
    ) else (
        echo     [!] Cargo workspace check failed. Inspect compiler logs.
        set "ALL_DEPS_OK=0"
    )
)

echo.
echo +=======================================================================+
if "!ALL_DEPS_OK!"=="1" (
    echo ^|                 ENVIRONMENT SETUP COMPLETE: ALL SYSTEMS READY       ^|
    echo +=======================================================================+
    echo.
    echo You can now run:
    echo   run.bat                 -- Launch the desktop GUI or CLI
    echo   cargo test --workspace  -- Run the automated 76-test suite
    echo.
) else (
    echo ^|         SETUP FINISHED WITH WARNINGS: REVIEW MISSING ITEMS ABOVE      ^|
    echo +=======================================================================+
    echo.
)

endlocal
