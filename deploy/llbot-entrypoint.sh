#!/bin/sh
# Keep upstream configuration intact; optionally resume the account chosen by
# the administrator after a successful QR login.
set -eu
umask 077
cd /app/llbot
set -- node --enable-source-maps ./llbot.js
if [ -f data/auto_login_qq ]; then
  account=$(cat data/auto_login_qq)
  case "$account" in
    ""|*[!0-9]*) echo "Invalid data/auto_login_qq; expected a QQ number" >&2; exit 2 ;;
  esac
  if [ "${#account}" -gt 20 ]; then
    echo "Invalid data/auto_login_qq; number is too long" >&2
    exit 2
  fi
  set -- "$@" -q "$account"
fi
exec "$@"
