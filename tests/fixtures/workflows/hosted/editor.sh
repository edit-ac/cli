#!/bin/sh
mode=$1
payload=$2
signal=$3
buffer=$4
case "$mode" in
 changed) /bin/cp "$payload" "$buffer" ;;
 noop) exit 0 ;;
 cancel) /bin/cp "$payload" "$buffer"; exit 130 ;;
 fail) /bin/cp "$payload" "$buffer"; exit 7 ;;
 invalid) printf '\377' > "$buffer" ;;
 *) exit 8 ;;
esac
printf 'editor completed\n' > "$signal"
