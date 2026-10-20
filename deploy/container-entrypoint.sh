#!/bin/sh
set -eu
umask 077

# Allow utilities such as tar without starting the bot or touching its config.
case "${1:-}" in
  ""|--*) ;;
  *) exec "$@" ;;
esac

config=${QKBOT_CONFIG_PATH:-/data/config.toml}
template=${QKBOT_CONFIG_TEMPLATE:-/usr/share/qkbot/config.example.toml}
mode=${QKBOT_CONTAINER_NETWORK:-host}
case "$mode" in
  host) listen=127.0.0.1:3210 ;;
  pod) listen=0.0.0.0:3210 ;;
  *) echo "QKBOT_CONTAINER_NETWORK must be host or pod" >&2; exit 2 ;;
esac

if [ ! -e "$config" ]; then
  directory=$(dirname "$config")
  mkdir -p "$directory"
  temporary=$(mktemp "$directory/.qkbot-init-XXXXXX")
  trap 'rm -f "$temporary"' EXIT HUP INT TERM
  awk -v listen="$listen" '
    /^\[web\]/ { in_web=1 }
    /^\[/ && !/^\[web\]/ { in_web=0 }
    in_web && /^listen[[:space:]]*=/ {
      print "listen = \"" listen "\""
      next
    }
    { print }
  ' "$template" > "$temporary"
  # Exclusive creation: simultaneous starts cannot replace somebody else's config.
  ln "$temporary" "$config"
  rm -f "$temporary"
  trap - EXIT HUP INT TERM
  echo "Created $config; configure the bot through the administrator page." >&2
fi

if [ "${1:-}" = "--init-config" ]; then
  exit 0
fi
exec qkbot --config "$config" "$@"
