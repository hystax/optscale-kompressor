#!/bin/sh
# Finish a kompressor install by hand, for the case the package could not: no
# console user at install time (an unattended or remote install), or the agent
# failed to load. Run it as the user who will use the proxy, not with sudo.
set -e
SHARE=/usr/local/share/kompressor
LABEL=com.kompressor.proxy
CONF_DIR="$HOME/.config/kompressor"; CONF="$CONF_DIR/config.toml"
STATE="$HOME/Library/Application Support/kompressor"
LOGS="$HOME/Library/Logs"; AGENTS="$HOME/Library/LaunchAgents"
PLIST="$AGENTS/$LABEL.plist"

[ "$(id -u)" -ne 0 ] || { echo "run this as yourself, not with sudo" >&2; exit 2; }
mkdir -p "$CONF_DIR" "$STATE" "$LOGS" "$AGENTS"
[ -f "$CONF" ] || { sed "s|@STATE@|$STATE|g" "$SHARE/config.toml.in" > "$CONF"; chmod 600 "$CONF"; }
sed -e "s|@CONFIG@|$CONF|g" -e "s|@LOGS@|$LOGS|g" -e "s|@STATE@|$STATE|g" "$SHARE/$LABEL.plist.in" > "$PLIST"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "loaded. config $CONF, logs $LOGS/kompressor.log"
