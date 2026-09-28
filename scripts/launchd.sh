#!/bin/bash
# Install / remove `baton serve` as a login LaunchAgent — the developer
# variant with versioned releases. Users run `baton setup` instead (same
# label, plain binary path). Listen/upstream come from
# ~/.config/baton/config.toml unless given here (docs/config.md).
#
#   scripts/launchd.sh install <path-to-baton-binary> [--listen HOST:PORT] [--upstream URL]
#   scripts/launchd.sh uninstall
#   scripts/launchd.sh status
#
# Releases live under ~/Library/Application Support/baton/releases/<id>/baton
# with a `current` symlink; the agent always runs `current`, so switching or
# rolling back a release is a symlink change plus `launchctl kickstart`.
set -euo pipefail
LABEL=com.baton.daemon
BASE="$HOME/Library/Application Support/baton"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

cmd="${1:-status}"; shift || true
case "$cmd" in
  install)
    bin="${1:?binary path}"; shift
    listen=""; upstream=""
    while [[ $# -gt 0 ]]; do
      case "$1" in --listen) listen="$2"; shift 2;; --upstream) upstream="$2"; shift 2;; *) echo "unknown $1"; exit 2;; esac
    done
    extra=""
    [[ -n "$listen" ]] && extra+="
    <string>--listen</string><string>$listen</string>"
    [[ -n "$upstream" ]] && extra+="
    <string>--upstream</string><string>$upstream</string>"
    id="$(date +%Y%m%d-%H%M%S)-$( ("$bin" 2>&1 || true) | head -1 | awk '{print $2}')"
    mkdir -p "$BASE/releases/$id"
    cp "$bin" "$BASE/releases/$id/baton"
    ln -sfn "$BASE/releases/$id" "$BASE/current"
    cat > "$PLIST" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array>
    <string>$BASE/current/baton</string><string>serve</string>$extra
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>5</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/baton-daemon.out.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/baton-daemon.err.log</string>
</dict></plist>
PL
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    launchctl bootstrap "$DOMAIN" "$PLIST"
    echo "installed $id → ${listen:-config listen} (upstream ${upstream:-from config})"
    ;;
  uninstall)
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    rm -f "$PLIST"
    echo "removed $LABEL (releases kept in $BASE)"
    ;;
  status)
    launchctl print "$DOMAIN/$LABEL" 2>/dev/null | awk -F'= ' '/state =|pid =|last exit code/ {print "  " $0}' | head -4 || echo "not loaded"
    [[ -L "$BASE/current" ]] && echo "  current: $(basename "$(readlink "$BASE/current")")"
    ;;
  *) echo "usage: $0 install <bin> [--listen ..] [--upstream ..] | uninstall | status"; exit 2;;
esac
