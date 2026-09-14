#!/bin/sh
# Build logi, install it to ~/.local/bin, and (re)start the watch user service
set -eu
cd "$(dirname "$0")"

cargo build --release
install -Dm755 target/release/logi ~/.local/bin/logi
install -Dm644 logi-watch.service ~/.config/systemd/user/logi-watch.service

systemctl --user daemon-reload
systemctl --user enable logi-watch.service
# restart rather than start, so that a running watcher picks up the new binary
systemctl --user restart logi-watch.service
systemctl --user --no-pager status logi-watch.service
