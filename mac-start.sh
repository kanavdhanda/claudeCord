#!/bin/bash
# Claude Discord Bot - Launcher
# Usage:
#   ./mac-start.sh          → Launch the panel (panel manages the bot)
#   ./mac-start.sh --fg     → Run the bot in the foreground (called by the panel)
#   ./mac-start.sh --stop   → Stop everything (panel + bot)
#   ./mac-start.sh --status → Check status

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="$SCRIPT_DIR/.env"

LABEL="com.claude-discord"
PLIST_DST="$HOME/Library/LaunchAgents/$LABEL.plist"
MENUBAR="$SCRIPT_DIR/menubar/ClaudeBotMenu"
MENUBAR_LABEL="com.claude-discord-menubar"
MENUBAR_PLIST_DST="$HOME/Library/LaunchAgents/$MENUBAR_LABEL.plist"

# --stop: kill panel and bot
if [ "$1" = "--stop" ]; then
    UID_=$(id -u)
    # Stop legacy launchd-managed bot if any (migration safety)
    launchctl bootout "gui/$UID_/$LABEL" 2>/dev/null
    launchctl unload "$PLIST_DST" 2>/dev/null
    launchctl remove "$LABEL" 2>/dev/null
    rm -f "$PLIST_DST"  # clean legacy plist
    # Stop bot process
    pkill -KILL -f "node dist/index.js" 2>/dev/null
    rm -f "$SCRIPT_DIR/.bot.lock"
    # Stop panel
    launchctl bootout "gui/$UID_/$MENUBAR_LABEL" 2>/dev/null
    launchctl unload "$MENUBAR_PLIST_DST" 2>/dev/null
    pkill -KILL -f "ClaudeBotMenu" 2>/dev/null
    echo "🔴 Stopped"
    exit 0
fi

# --status: report bot state from .bot.lock
if [ "$1" = "--status" ]; then
    if [ -f "$SCRIPT_DIR/.bot.lock" ]; then
        PID=$(cat "$SCRIPT_DIR/.bot.lock" 2>/dev/null)
        if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
            echo "🟢 Bot running (PID: $PID)"
            exit 0
        fi
    fi
    echo "🔴 Bot stopped"
    exit 0
fi

# --fg: run the bot in the foreground (called by the panel)
if [ "$1" = "--fg" ]; then
    export NVM_DIR="$HOME/.nvm"
    [ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh"

    if ! command -v node &>/dev/null; then
        for p in /opt/homebrew/bin /usr/local/bin "$HOME/.nodenv/shims" "$HOME/.fnm/aliases/default/bin"; do
            if [ -x "$p/node" ]; then
                export PATH="$p:$PATH"
                break
            fi
        done
    fi

    if ! command -v node &>/dev/null; then
        echo "[claude-bot] ERROR: node not found. Install Node.js (nvm, homebrew, or nodejs.org)"
        exit 1
    fi

    echo "[claude-bot] Using node: $(which node) ($(node --version))"
    cd "$SCRIPT_DIR"

    VERSION=$(git describe --tags --always 2>/dev/null || echo "unknown")
    echo "[claude-bot] Current version: $VERSION"

    if [ ! -d "node_modules" ]; then
        echo "[claude-bot] Installing dependencies..."
        npm install
    fi

    if [ ! -d "dist" ]; then
        echo "[claude-bot] Building..."
        npm run build
    elif find src -name "*.ts" -newer dist/index.js 2>/dev/null | grep -q .; then
        echo "[claude-bot] Source changed, rebuilding..."
        npm run build
    fi

    if ! node -e "require('./node_modules/better-sqlite3/build/Release/better_sqlite3.node')" 2>/dev/null; then
        echo "[claude-bot] Native modules incompatible, rebuilding..."
        npm rebuild better-sqlite3
    fi

    echo "[claude-bot] Starting bot (foreground)..."
    trap 'rm -f "$SCRIPT_DIR/.bot.lock"' EXIT
    exec node dist/index.js
fi

# Default: launch the panel.
# The panel auto-starts the bot, manages it, and stops it on quit.

# One-time migration: remove legacy launchd-managed bot plist
if [ -f "$PLIST_DST" ]; then
    UID_=$(id -u)
    launchctl bootout "gui/$UID_/$LABEL" 2>/dev/null
    launchctl unload "$PLIST_DST" 2>/dev/null
    launchctl remove "$LABEL" 2>/dev/null
    rm -f "$PLIST_DST"
    pkill -KILL -f "node dist/index.js" 2>/dev/null
fi

# Compile menu bar app if needed
if [ -f "$SCRIPT_DIR/menubar/ClaudeBotMenu.swift" ]; then
    if [ ! -f "$MENUBAR" ] || [ "$SCRIPT_DIR/menubar/ClaudeBotMenu.swift" -nt "$MENUBAR" ]; then
        if ! xcode-select -p &>/dev/null; then
            echo "⚠ Xcode Command Line Tools required. Installing..."
            xcode-select --install
            echo "  Complete the installation dialog, then re-run this script."
            exit 0
        fi
        if ! xcrun --find swiftc &>/dev/null; then
            echo "⚠ Xcode license not accepted. Accepting..."
            sudo xcodebuild -license accept 2>/dev/null || {
                echo "  Failed. Please run manually: sudo xcodebuild -license accept"
                exit 1
            }
        fi
        echo "🔨 Building menu bar app..."
        swiftc -o "$MENUBAR" "$SCRIPT_DIR/menubar/ClaudeBotMenu.swift" -framework Cocoa
    fi
fi

# Kill any previous panel instance and launch a fresh one
pkill -KILL -f "ClaudeBotMenu" 2>/dev/null
sleep 1

if [ -f "$MENUBAR" ]; then
    nohup "$MENUBAR" > /dev/null 2>&1 &
    disown
    echo "🟢 Panel launched (panel will auto-start the bot)"
else
    echo "❌ Menubar binary not found"
    exit 1
fi

# Register menu bar app for autostart on login/reboot
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$MENUBAR_PLIST_DST" <<MBEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$MENUBAR_LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$MENUBAR</string>
    </array>
    <key>WorkingDirectory</key>
    <string>$SCRIPT_DIR</string>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/dev/null</string>
    <key>StandardErrorPath</key>
    <string>/dev/null</string>
</dict>
</plist>
MBEOF

echo "🔔 Panel autostart registered (will launch on login)"
echo "   Stop:   ./mac-start.sh --stop"
echo "   Status: ./mac-start.sh --status"
echo "   Log:    tail -f bot.log"
