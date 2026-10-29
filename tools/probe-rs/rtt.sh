#!/bin/bash
# Streams the kernel's RTT log channel through probe-rs and decodes it with
# t5log (doc/12 L2). probe-rs stays in the foreground so Ctrl-C stops it;
# the decoder follows the file probe-rs writes (its stdout would stay
# buffered, because a binary channel is printed as hex without newlines).
#
#   tools/probe-rs/rtt.sh PROBE_RS PROBE KERNEL_ELF SCHEMAS T5LOG OUT_HEX
set -u
probe_rs=$1 probe=$2 elf=$3 schemas=$4 t5log=$5 out=$6
here=$(dirname "$0")

: > "$out"
tail -n +1 -F "$out" 2>/dev/null | "$t5log" decode --hex --pretty --schemas "$schemas" &
decoder=$!
trap 'kill $decoder 2>/dev/null; pkill -P $$ tail 2>/dev/null' EXIT

"$probe_rs" attach --non-interactive --probe "$probe" --chip esp32p4 \
    --chip-description-path "$here/esp32p4-tab5.yaml" \
    --target-output-file "$out" "$elf" > "${out%.hex}-probe-rs.log" 2>&1
status=$?
sleep 0.5 # let the decoder catch up with the last bytes
exit $status
