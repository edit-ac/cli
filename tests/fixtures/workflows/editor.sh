#!/bin/sh
mode=$1
payload=$2
target=$3
buffer=$4
case "$mode" in
 changed) /bin/cp "$payload" "$buffer" ;;
 noop) exit 0 ;;
 cancel) /bin/cp "$payload" "$buffer"; exit 130 ;;
 fail) /bin/cp "$payload" "$buffer"; exit 7 ;;
 stale) /bin/cp "$payload" "$buffer"; printf 'external writer\n' > "$target" ;;
 replace) /bin/cp "$payload" "$buffer"; /bin/rm "$target"; printf 'replacement\n' > "$target" ;;
 interrupt) /bin/cp "$payload" "$buffer"; kill -TERM "$PPID" ;;
 invalid) printf '\377' > "$buffer" ;;
 *) exit 8 ;;
esac
