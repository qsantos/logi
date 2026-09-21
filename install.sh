#!/bin/sh
# Build logi, install it to ~/.local/bin, and bind the host switch to Super+K
set -eu
cd "$(dirname "$0")"

cargo build --release
install -Dm755 target/release/logi ~/.local/bin/logi

# No argument on purpose: each host reads which input is live and moves everything to the other one,
# so the very same binding does the opposite thing on the other machine
key='/commands/custom/<Super>k'
cmd="$HOME/.local/bin/logi switch"
xfconf-query -c xfce4-keyboard-shortcuts -p "$key" -n -t string -s "$cmd" 2>/dev/null ||
    xfconf-query -c xfce4-keyboard-shortcuts -p "$key" -s "$cmd"
xfconf-query -c xfce4-keyboard-shortcuts -p "$key"
