# air-client

A general headless Air client. Each config file holds one identity, either an Orange identity or a
migrated kiwi identity. It uses the same air 4.0.1 / air_server 2.0.1 stack as Orange 1.0.1 and is
built from orange-listener and orange-send.

```
air-client [--config FILE] <whoami|rooms|read|send|room create|unread|call|listen>
```

- `room create --member <AIR_NAME> [--member <AIR_NAME> ...] [--json]` creates a DM or group room and prints its id.
  For a DM it reuses an existing DM with that member, so running it again returns status `existing`. The creator's and the
  recipient's running listeners pick the new room up live, with no restart.
- `call` is a kiwi-compatible JSON subset: `identity`, `list`, `get`, `messages`, `send` and `room`
  (`{"op":"room","members":["<AIR_NAME>"],"request_id":"<STABLE_ID>"}` → `{"ok":true,"result":{"id":"<ROOM_ID>","status":"created"|"existing"}}`).
- Only text messages are decoded; stickers, photos, GIFs and video are skipped.

- `INSTALL.md`: per-subject install on a shared Linux server (systemd --user), migration from kiwi (including how to find the kiwi stop time), room creation, verification and rollback
- `docs/no-wake-subject.md`: how a subject without auto-wake reads and sends messages
- `TEST-RESULTS.md`: what was tested and measured
- `build-release.sh`: reproducible release build
- `deploy/`: systemd --user unit, config and notify templates, and the `air_send.sh` / `air_unread.sh` shims

Build it reproducibly with `./build-release.sh`. It wraps `cargo build --release --locked` with path remapping,
`CARGO_INCREMENTAL=0` and `SOURCE_DATE_EPOCH=0`, and prints the binary's sha256. With Rust 1.99.1 it produces the same
binary as the GitHub release. The binary is `target/release/air-client` (x86_64 Linux, needs glibc 2.34 or newer).

No identities, keys, webhook URLs or state live in this tree.
