@echo off
setlocal enabledelayedexpansion

:: Claude Discord Bot - Windows Auto-update & Start Script
:: Usage:
::   win-start.bat          - background mode + tray app
::   win-start.bat --fg     - foreground mode (debug)
::   win-start.bat --stop   - stop bot
::   win-start.bat --status - check status

set "SCRIPT_DIR=%~dp0"
set "SCRIPT_DIR=%SCRIPT_DIR:~0,-1%"
set "ENV_FILE=%SCRIPT_DIR%\.env"
set "TRAY_EXE=%SCRIPT_DIR%\tray\ClaudeBotTray.exe"
set "TRAY_SRC=%SCRIPT_DIR%\tray\ClaudeBotTray.cs"

:: Check node
where node >nul 2>&1
if errorlevel 1 (
    echo Node.js not found. Please run install.bat first.
    pause
    exit /b 1
)

:: --stop
if "%~1"=="--stop" (
    for /f "tokens=2" %%a in ('tasklist /fi "windowtitle eq ClaudeDiscordBot" /fo list 2^>nul ^| findstr "PID"') do (
        taskkill /pid %%a /f >nul 2>&1
    )
    wmic process where "commandline like '%%dist/index.js%%' and name='node.exe'" call terminate >nul 2>&1
    del "%SCRIPT_DIR%\.bot.lock" >nul 2>&1
    taskkill /im ClaudeBotTray.exe /f >nul 2>&1
    echo Bot stopped
    exit /b 0
)

:: --status
if "%~1"=="--status" (
    if exist "%SCRIPT_DIR%\.bot.lock" (
        echo Bot is running
    ) else (
        echo Bot is stopped
    )
    exit /b 0
)

:: --fg: foreground mode
if "%~1"=="--fg" (
    cd /d "%SCRIPT_DIR%"

    for /f %%i in ('git describe --tags --always 2^>nul') do set "VERSION=%%i"
    if "!VERSION!"=="" set "VERSION=unknown"
    echo [claude-bot] Version: !VERSION!
    echo [claude-bot] Checking for updates...
    git fetch origin main >nul 2>&1

    for /f %%i in ('git rev-parse HEAD 2^>nul') do set "LOCAL=%%i"
    for /f %%i in ('git rev-parse origin/main 2^>nul') do set "REMOTE=%%i"

    if not "!LOCAL!"=="!REMOTE!" (
        if not "!LOCAL!"=="" (
            if not "!REMOTE!"=="" (
                echo [claude-bot] Update available (use tray to update^)
            )
        )
    ) else (
        echo [claude-bot] Up to date
    )

    if not exist "dist" (
        echo [claude-bot] No build files found, building...
        call npm run build
    ) else (
        for /f %%t in ('powershell -NoProfile -Command "(Get-ChildItem src -Recurse -Filter *.ts | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime.Ticks"') do set "SRC_TIME=%%t"
        for /f %%t in ('powershell -NoProfile -Command "(Get-Item dist\index.js).LastWriteTime.Ticks"') do set "DIST_TIME=%%t"
        if !SRC_TIME! gtr !DIST_TIME! (
            echo [claude-bot] Source changed, rebuilding...
            call npm run build
        )
    )

    :: Check native module compatibility
    node -e "require('./node_modules/better-sqlite3/build/Release/better_sqlite3.node')" 2>nul
    if errorlevel 1 (
        echo [claude-bot] Native modules incompatible, rebuilding...
        call npm rebuild better-sqlite3
    )

    echo [claude-bot] Starting bot (foreground^)...
    node dist/index.js
    exit /b 0
)

:: Default: background mode
cd /d "%SCRIPT_DIR%"

:: Stop existing bot if running
if exist "%SCRIPT_DIR%\.bot.lock" (
    echo Stopping existing bot...
    for /f "tokens=2" %%a in ('tasklist /fi "windowtitle eq ClaudeDiscordBot" /fo list 2^>nul ^| findstr "PID"') do (
        taskkill /pid %%a /f >nul 2>&1
    )
    wmic process where "commandline like '%%dist/index.js%%' and name='node.exe'" call terminate >nul 2>&1
    del "%SCRIPT_DIR%\.bot.lock" >nul 2>&1
    timeout /t 2 /nobreak >nul
)

:: Check for updates (manual update via tray)
echo [claude-bot] Checking for updates...
git fetch origin main >nul 2>&1
for /f %%i in ('git rev-parse HEAD 2^>nul') do set "LOCAL=%%i"
for /f %%i in ('git rev-parse origin/main 2^>nul') do set "REMOTE=%%i"

if not "!LOCAL!"=="!REMOTE!" (
    if not "!LOCAL!"=="" (
        if not "!REMOTE!"=="" (
            echo [claude-bot] Update available (use tray to update^)
        )
    )
) else (
    echo [claude-bot] Up to date
)

if not exist "dist" (
    echo [claude-bot] No build files found, building...
    call npm run build
) else (
    for /f %%t in ('powershell -NoProfile -Command "(Get-ChildItem src -Recurse -Filter *.ts | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime.Ticks"') do set "SRC_TIME=%%t"
    for /f %%t in ('powershell -NoProfile -Command "(Get-Item dist\index.js).LastWriteTime.Ticks"') do set "DIST_TIME=%%t"
    if !SRC_TIME! gtr !DIST_TIME! (
        echo [claude-bot] Source changed, rebuilding...
        call npm run build
    )
)

:: Check native module compatibility
node -e "require('./node_modules/better-sqlite3/build/Release/better_sqlite3.node')" 2>nul
if errorlevel 1 (
    echo [claude-bot] Native modules incompatible, rebuilding...
    call npm rebuild better-sqlite3
)

:: Compile tray app if exe not found or source is newer
set "NEED_TRAY_BUILD=0"
if not exist "%TRAY_EXE%" (
    set "NEED_TRAY_BUILD=1"
) else (
    for /f %%a in ('powershell -NoProfile -Command "if ((Get-Item \"%TRAY_SRC%\").LastWriteTime -gt (Get-Item \"%TRAY_EXE%\").LastWriteTime) { echo 1 } else { echo 0 }"') do set "NEED_TRAY_BUILD=%%a"
)
if "!NEED_TRAY_BUILD!"=="1" (
    if exist "%TRAY_SRC%" (
        echo Building tray app...
        set "CSC="
        for /f "delims=" %%i in ('dir /b /s "%WINDIR%\Microsoft.NET\Framework64\csc.exe" 2^>nul') do set "CSC=%%i"
        if "!CSC!"=="" (
            for /f "delims=" %%i in ('dir /b /s "%WINDIR%\Microsoft.NET\Framework\csc.exe" 2^>nul') do set "CSC=%%i"
        )
        if not "!CSC!"=="" (
            "!CSC!" /nologo /target:winexe /out:"%TRAY_EXE%" /reference:System.Windows.Forms.dll /reference:System.Drawing.dll "%TRAY_SRC%"
            if not exist "%TRAY_EXE%" (
                echo Tray app build failed
            )
        ) else (
            echo C# compiler not found
        )
    )
)

:: Start tray app (--show opens control panel)
if exist "%TRAY_EXE%" (
    taskkill /im ClaudeBotTray.exe /f >nul 2>&1
    start "" "%TRAY_EXE%" --show
)

:: Start bot if .env exists
if exist "%ENV_FILE%" (
    echo Set ws = CreateObject^("WScript.Shell"^) > "%SCRIPT_DIR%\.bot-run.vbs"
    echo ws.Run "cmd /c cd /d %SCRIPT_DIR% ^& echo running^> .bot.lock ^& node dist/index.js ^& del .bot.lock", 0, False >> "%SCRIPT_DIR%\.bot-run.vbs"
    wscript "%SCRIPT_DIR%\.bot-run.vbs"
    del "%SCRIPT_DIR%\.bot-run.vbs" >nul 2>&1
    echo Bot started in background
) else (
    echo .env not found. Please configure settings from the tray icon.
)
echo    Stop: win-start.bat --stop
echo    Status: win-start.bat --status
echo    Log: type bot.log
