#!/usr/bin/env bash
# Install zcrawl for the current user, without root.
#
#   contrib/install-user.sh            install the binary and config only
#   contrib/install-user.sh systemd    also install and start a systemd user service
#   contrib/install-user.sh cron       also add crontab entries that keep it running
#
# Layout:
#   ~/.local/bin/zcrawl                  binary
#   ~/.config/zcrawl/zcrawl.toml         config (an existing one is kept)
#   ~/.local/share/zcrawl/               database, lock files, cron log
#
# Run `cargo build --release` first. Running the script again upgrades the
# binary; restart zcrawl afterwards to use it.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
mode="${1:-}"
bin_dir="$HOME/.local/bin"
conf_dir="$HOME/.config/zcrawl"
data_dir="$HOME/.local/share/zcrawl"
conf="$conf_dir/zcrawl.toml"

case "$mode" in
  "" | systemd | cron) ;;
  *) echo "usage: $0 [systemd|cron]" >&2; exit 2 ;;
esac

if [[ ! -x "$repo/target/release/zcrawl" ]]; then
  echo "no release build found; run: cargo build --release" >&2
  exit 1
fi

mkdir -p "$bin_dir" "$conf_dir" "$data_dir"
install -m 0755 "$repo/target/release/zcrawl" "$bin_dir/zcrawl"
echo "installed $bin_dir/zcrawl"

if [[ -e "$conf" ]]; then
  echo "kept existing $conf"
else
  # TOML does not expand ~, so write the absolute database path.
  sed "s|^database = .*|database = \"$data_dir/zcrawl.sqlite\"|" \
    "$repo/contrib/zcrawl.toml" > "$conf"
  echo "wrote $conf"
fi

case "$mode" in
  systemd)
    unit_dir="$HOME/.config/systemd/user"
    mkdir -p "$unit_dir"
    install -m 0644 "$repo/contrib/systemd/zcrawl-user.service" "$unit_dir/zcrawl.service"
    systemctl --user daemon-reload
    systemctl --user enable --now zcrawl
    echo "started; follow with: journalctl --user -u zcrawl -f"
    if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || true)" != "yes" ]]; then
      if loginctl enable-linger "$USER" 2>/dev/null; then
        echo "enabled lingering: zcrawl keeps running after you log out"
      else
        echo "WARNING: could not enable lingering; zcrawl will stop when you log out." >&2
        echo "         Ask an admin to run 'loginctl enable-linger $USER', or use: $0 cron" >&2
      fi
    fi
    ;;
  cron)
    command -v flock >/dev/null || { echo "flock is required for the cron setup" >&2; exit 1; }
    if crontab -l 2>/dev/null | grep -q 'zcrawl/cron.lock'; then
      echo "crontab already has zcrawl entries; left unchanged"
    else
      (crontab -l 2>/dev/null || true; cat "$repo/contrib/cron/zcrawl.crontab") | crontab -
      echo "added crontab entries; zcrawl starts within 10 minutes"
    fi
    echo "follow with: tail -f $data_dir/zcrawl.log"
    ;;
  "")
    echo "start it with: $bin_dir/zcrawl --config $conf run"
    ;;
esac
