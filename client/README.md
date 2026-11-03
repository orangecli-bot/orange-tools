# air-client

A general headless Air client. Each config file holds one identity, either an Orange identity or a
migrated kiwi identity. It uses the same air 4.0.1 / air_server 2.0.1 stack as Orange 1.0.1 and is
built from orange-listener and orange-send.

```
air-client [--config FILE] <whoami|rooms|read|send|unread|call|listen>
```

- `INSTALL.md`: per-subject install on a shared Linux server (systemd --user), migration from kiwi, verification and rollback
- `docs/no-wake-subject.md`: how a subject without auto-wake reads and sends messages
- `TEST-RESULTS.md`: what was tested and measured
- `deploy/`: systemd --user unit, config and notify templates, and the `air_send.sh` / `air_unread.sh` shims

Build it with `cargo build --release --locked`. The binary is `target/release/air-client`.

No identities, keys, webhook URLs or state live in this tree.
