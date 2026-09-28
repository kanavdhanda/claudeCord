@echo off
chcp 65001 >nul 2>&1
setlocal enabledelayedexpansion

echo ===================================
echo  Claude Code Discord Bot Installer
echo ===================================
echo.

set NEED_LOGIN=0

:: --- 1. Node.js ---
echo [1/4] Checking Node.js...
where node >nul 2>&1
if %errorlevel% neq 0 (
    echo   Node.js not found. Installing...
    where winget >nul 2>&1
    if %errorlevel% equ 0 (
        winget install OpenJS.NodeJS.LTS --accept-source-agreements --accept-package-agreements
        echo   Node.js installed. Restarting in new terminal...
        start cmd /c "cd /d "%SCRIPT_DIR%" && install.bat"
        exit /b 0
    )
    echo   winget not available. Downloading Node.js installer...
    set "NODE_MSI=%TEMP%\node-install.msi"
    powershell -Command "Invoke-WebRequest -Uri 'https://nodejs.org/dist/v22.14.0/node-v22.14.0-x64.msi' -OutFile '!NODE_MSI!'" 2>nul
    if exist "!NODE_MSI!" (
        echo   Installing Node.js (this may take a moment^)...
        msiexec /i "!NODE_MSI!" /passive /norestart
        del "!NODE_MSI!" >nul 2>&1
        echo   Node.js installed. Restarting in new terminal...
        start cmd /c "cd /d "%SCRIPT_DIR%" && install.bat"
        exit /b 0
    ) else (
        echo   X Download failed.
        echo   Download Node.js manually from https://nodejs.org
        echo   After installing, restart this script.
        pause
        exit /b 1
    )
)

for /f "tokens=1 delims=." %%a in ('node -v') do set NODE_MAJOR=%%a
set NODE_MAJOR=%NODE_MAJOR:v=%
if %NODE_MAJOR% lss 20 (
    echo   ! Node.js 20+ required. Current: v%NODE_MAJOR%
    echo   Upgrading...
    where winget >nul 2>&1
    if %errorlevel% equ 0 (
        winget upgrade OpenJS.NodeJS.LTS --accept-source-agreements --accept-package-agreements
        echo   Node.js upgraded. Restarting in new terminal...
        start cmd /c "cd /d "%SCRIPT_DIR%" && install.bat"
        exit /b 0
    )
    echo   winget not available. Downloading Node.js installer...
    set "NODE_MSI=%TEMP%\node-install.msi"
    powershell -Command "Invoke-WebRequest -Uri 'https://nodejs.org/dist/v22.14.0/node-v22.14.0-x64.msi' -OutFile '!NODE_MSI!'" 2>nul
    if exist "!NODE_MSI!" (
        echo   Upgrading Node.js (this may take a moment^)...
        msiexec /i "!NODE_MSI!" /passive /norestart
        del "!NODE_MSI!" >nul 2>&1
        echo   Node.js upgraded. Restarting in new terminal...
        start cmd /c "cd /d "%SCRIPT_DIR%" && install.bat"
        exit /b 0
    ) else (
        echo   X Download failed. Download from https://nodejs.org
        pause
        exit /b 1
    )
)

for /f "tokens=*" %%v in ('node -v') do echo   Found Node.js %%v
echo   OK
echo.

:: --- 2. Claude Code CLI ---
echo [2/4] Checking Claude Code CLI...
:: Ensure npm global bin is in PATH
set "PATH=%PATH%;%APPDATA%\npm"
where claude >nul 2>&1
if %errorlevel% neq 0 (
    echo   Claude Code not found. Installing...
    call npm install -g @anthropic-ai/claude-code
    :: Verify by checking if claude exists, not errorlevel
    where claude >nul 2>&1
    if !errorlevel! neq 0 (
        echo   X Failed to install Claude Code.
        pause
        exit /b 1
    )
    echo   OK Claude Code installed
    echo.
    echo   ! Claude Code login required!
    echo   Run 'claude' once to complete OAuth login.
    set NEED_LOGIN=1
) else (
    echo   OK Found Claude Code
)
echo.

:: --- 3. npm install ---
echo [3/4] Installing project dependencies...
call npm install
if %errorlevel% neq 0 (
    echo   X npm install failed.
    echo   If better-sqlite3 fails, install Visual Studio Build Tools:
    echo   winget install Microsoft.VisualStudio.2022.BuildTools
    echo   Then select "Desktop development with C++" workload.
    pause
    exit /b 1
)
echo   OK Done
echo.

:: --- 4. .env ---
echo [4/4] Checking .env file...
if exist .env (
    echo   .env already exists
    echo   OK
) else (
    echo   .env not found - tray app will open Settings dialog on first launch
)
echo.

:: --- 5. Build ---
echo [5/6] Building project...
call npm run build
if %errorlevel% neq 0 (
    echo   X Build failed.
    pause
    exit /b 1
)
echo   OK Done
echo.

:: --- 6. Desktop shortcut ---
echo [6/6] Creating desktop shortcut...
set "SCRIPT_DIR=%~dp0"
set "SCRIPT_DIR=%SCRIPT_DIR:~0,-1%"
set "SHORTCUT_VBS=%TEMP%\create-shortcut.vbs"
for /f "usebackq delims=" %%D in (`powershell -NoProfile -Command "[Environment]::GetFolderPath('Desktop')"`) do set "DESKTOP=%%D"

echo Set oWS = WScript.CreateObject("WScript.Shell") > "%SHORTCUT_VBS%"
echo sLinkFile = "%DESKTOP%\Claude Discord Bot.lnk" >> "%SHORTCUT_VBS%"
echo Set oLink = oWS.CreateShortcut(sLinkFile) >> "%SHORTCUT_VBS%"
echo oLink.TargetPath = "%SCRIPT_DIR%\win-start.bat" >> "%SHORTCUT_VBS%"
echo oLink.WorkingDirectory = "%SCRIPT_DIR%" >> "%SHORTCUT_VBS%"
echo oLink.Description = "Claude Discord Bot" >> "%SHORTCUT_VBS%"
echo oLink.IconLocation = "%SCRIPT_DIR%\docs\icon.ico, 0" >> "%SHORTCUT_VBS%"
echo oLink.WindowStyle = 7 >> "%SHORTCUT_VBS%"
echo oLink.Save >> "%SHORTCUT_VBS%"
cscript //nologo "%SHORTCUT_VBS%" >nul 2>&1
del "%SHORTCUT_VBS%" >nul 2>&1

if exist "%DESKTOP%\Claude Discord Bot.lnk" (
    echo   OK Desktop shortcut created
) else (
    echo   ! Could not create desktop shortcut
)
echo.

:: --- Done ---
echo ===================================
echo  Installation complete!
echo ===================================
echo.
if %NEED_LOGIN%==1 (
    echo Next steps:
    echo   1. Run 'claude' to login to Claude Code
    echo   2. Configure settings from the tray icon
) else (
    echo Starting Claude Discord Bot...
    echo.
    start "" "%SCRIPT_DIR%\win-start.bat"
)
echo.
echo See SETUP.md for detailed instructions.
echo.
pause
