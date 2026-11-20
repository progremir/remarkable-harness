#!/bin/sh
# Rex: browse a Rex server's sessions and use their terminals.
# The server's Tailscale name goes in /home/root/.config/rexpaper/server.
cd "$(dirname "$0")"
killall rexpaper 2>/dev/null
exec ./rexpaper 2>/tmp/rexpaper.log
