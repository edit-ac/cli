#!/bin/sh
# Deterministic offline process fixture. Arguments are data, never shell code.
mode=$1
payload=$2
buffer=$3
case "$mode" in
  changed) cp "$payload" "$buffer" ;;
  noop) exit 0 ;;
  cancel) cp "$payload" "$buffer"; exit 130 ;;
  fail) cp "$payload" "$buffer"; exit 7 ;;
  signal-cancel) kill -INT $$ ;;
  signal-fail) kill -TERM $$ ;;
  invalid) printf '\377' > "$buffer" ;;
  oversized) dd if=/dev/zero of="$buffer" bs=1048577 count=1 2>/dev/null ;;
  missing) rm "$buffer" ;;
  directory) rm "$buffer"; mkdir "$buffer" ;;
  symlink) rm "$buffer"; ln -s "$payload" "$buffer" ;;
  record) printf '%s' "$payload" > "$buffer" ;;
  *) exit 8 ;;
esac
