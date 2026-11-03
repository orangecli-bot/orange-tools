#!/bin/sh
# air-client shim for the kiwi-era air_unread.sh: print new Air messages (one JSON line each:
# {"from","sender","room","timestamp_ns","content"}) or "no new messages", and mark them read (moved to read.jsonl).
# `air_unread.sh --json` prints one JSON array instead ([] when empty).
exec "$HOME/.local/bin/air-client" --config "$HOME/.config/air-client/air-client.env" unread "$@"
