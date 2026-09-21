#!/bin/sh
# Remove the logi binary, its keyboard shortcut, and the watch service older versions installed
set -eu

systemctl --user disable --now logi-watch.service 2>/dev/null || true
rm -f ~/.config/systemd/user/logi-watch.service ~/.local/bin/logi
systemctl --user daemon-reload
systemctl --user reset-failed logi-watch.service 2>/dev/null || true
xfconf-query -c xfce4-keyboard-shortcuts -p '/commands/custom/<Super>k' -r 2>/dev/null || true
