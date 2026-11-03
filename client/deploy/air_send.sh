#!/bin/sh
# air-client shim for the kiwi-era air_send.sh. Usage: air_send.sh <room_id> "text"   (or text on stdin)
# request_id is derived from room+text exactly like the kiwi-era scripts, so a retry never double-sends.
# The text goes over stdin, never argv. Prints {"ok":true,"result":{...}} or {"ok":false,"error":...} (exit 2).
R="$1"; T="${2:-$(cat)}"
[ -n "$R" ] || { echo "usage: air_send.sh <room_id> \"text\"" >&2; exit 1; }
RID="${AIR_SEND_PREFIX:-$(id -un)}-$(printf '%s|%s' "$R" "$T" | sha256sum | cut -c1-16)"
printf '%s' "$T" | exec "$HOME/.local/bin/air-client" --config "$HOME/.config/air-client/air-client.env" send "$R" - --request-id "$RID"
