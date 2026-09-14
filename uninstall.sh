#!/bin/sh
# Stop and remove the watch user service and the installed logi binary
set -eu

systemctl --user disable --now logi-watch.service 2>/dev/null || true
rm -f ~/.config/systemd/user/logi-watch.service ~/.local/bin/logi
systemctl --user daemon-reload
systemctl --user reset-failed logi-watch.service 2>/dev/null || true
