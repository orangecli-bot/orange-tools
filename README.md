# orange-tools

Headless command-line tools for an [Orange](https://github.com/ramp-stack) / Air identity. They use
the same `air` 4.0.1 / `air_server` 2.0.1 crates and the same wire format as the Orange 1.0.1 app
(ramp2 3.0.4, chk `ChatRoom` contract), so they work with existing Orange accounts and rooms without
the GUI.

| dir | binary | what it does |
|---|---|---|
| `listener/` | `orange-listener` | Long-running, event-driven listener. Keeps one resumable websocket to the Air server, is *pushed* each new message (no polling), and POSTs a JSON "wake" webhook for every new incoming text message. Persists its cursors so restarts neither replay history nor drop messages. |
| `send/` | `orange-send` | One-shot CLI: `whoami`, `rooms`, `read ROOM [--json]`, `send ROOM [TEXT\|-]`. |
| `client/` | `air-client` | General Air client: `listen` (push listener that writes unread.jsonl and can fire a wake webhook), `send`, `read`, `unread`, `rooms`, `whoami`, and `call` (a kiwi-compatible JSON subset). See `client/README.md`, `client/INSTALL.md` (multi-user systemd --user install) and `client/docs/no-wake-subject.md`. It is synced from an upstream working copy with `./sync.sh`. |

> Secrets: the identity file, all state files (they embed derived keys) and the wake key are kept in
> mode-600 files under mode-700 directories. The tools refuse group/other-readable secret files and
> never log identity material, message keys or the wake key.

## Build

Requires Rust 1.85+ (edition 2024). Each crate is standalone, with a pinned `Cargo.lock` that matches
Orange 1.0.1.

```sh
for c in listener send client; do (cd $c && cargo build --release --locked); done
# binaries: listener/target/release/orange-listener, send/target/release/orange-send, client/target/release/air-client
# (or set CARGO_TARGET_DIR to build outside the tree; target/ is gitignored)
```

The release binaries are stripped and only need libc, libm and libgcc_s. They have no GUI or GPU
dependencies.

## Identity

Both tools take the **root Orange identity secret** as a JSON file, in the same form Orange stores
under `ramp.air.identity`. Keep it `chmod 600`, never commit it (`identity/` and `*secret*` are
gitignored), and never paste it into chat or tickets. Move it between machines only as a file
(for example `scp` with `umask 077`).

## orange-send

```sh
export ORANGE_IDENTITY_FILE=<PATH_TO_IDENTITY_JSON>     # or --identity FILE (required)
export ORANGE_SEND_STATE=<PATH_TO_STATE_DIR>            # or --state DIR
                                                        # default: $XDG_STATE_HOME/orange-send or ~/.local/state/orange-send
orange-send whoami
orange-send rooms [--members]          # read-only catalog refresh (STORAGE channel reads only); prints room ids
orange-send read <ROOM_ID> -n 20 --json
orange-send send <ROOM_ID> "hello"     # or: orange-send send <ROOM_ID> - < message.txt
```

`send` prints one JSON line: `ok`, `room`, `server_ts_ns`, `message_id`, and timings.

**Send allowlist.** `send` only writes to rooms you have allowed. This guards against a script
posting to the wrong chat. Configure it in one of these ways:

- `ORANGE_SEND_ALLOWED_ROOMS="<ROOM_ID>,<ROOM_ID_2>"` (comma- or space-separated). Set it to `*` to allow any room.
- `<state>/allowed_rooms`: one room id per line, `#` comments allowed, `*` allows any room.

If neither is set, every `send` is refused. Room ids come from `orange-send rooms`.

## orange-listener

Configuration comes from a mode-600 env-style file (`--config FILE` or `ORANGE_LISTENER_CONFIG`).
Any key missing from that file is read from the process environment. Running with environment
variables only also works.

| key | meaning |
|---|---|
| `ORANGE_IDENTITY_FILE` | path to the identity JSON (mode 600) |
| `STATE_DIR` | listener state (`catalog.bin`, `watermark.json`), created mode 700/600 |
| `SEED_STATE_FILE` | optional: first-run seed of the Air catalog (postcard `air::Air`, mode 600) |
| `WAKE_ENV_FILE` | optional: separate mode-600 file that holds the two keys below |
| `AIR_WAKE_URL` | `https://<WAKE_HOST>/<WAKE_PATH>`, the webhook to POST to |
| `AIR_WAKE_KEY` | bearer token, sent as `Authorization: Bearer …` in-process over TLS (never in argv or logs) |

Templates: `listener/dist/orange-listener.env.example` and `listener/dist/wake.env.example`.

Webhook body:
`{"event":"orange_message","source":"orange-listener","me":"<AIR_NAME>","sender":"<AIR_NAME>","room":"<ROOM_ID>","content":"<text, at most 2000 chars>","ts":<unix s>,"ts_ns":<server ns>}`.
A failed POST is retried 6 times with backoff. Messages signed by your own identity, and non-text
actions (stickers, photos, …), do not trigger a wake. Run **only one** listener per identity, or
each message will wake twice.

### Install as a systemd --user service

```sh
install -D -m 755 listener/target/release/orange-listener ~/.local/bin/orange-listener
umask 077
mkdir -p ~/.config/orange-listener
cp listener/dist/orange-listener.env.example ~/.config/orange-listener/orange-listener.env
cp listener/dist/wake.env.example            ~/.config/orange-listener/wake.env
chmod 600 ~/.config/orange-listener/*.env
$EDITOR ~/.config/orange-listener/orange-listener.env ~/.config/orange-listener/wake.env   # fill in the <PLACEHOLDERS>
install -D -m 644 listener/dist/orange-listener.service ~/.config/systemd/user/orange-listener.service
systemctl --user daemon-reload
systemctl --user enable --now orange-listener
loginctl enable-linger "$USER"     # optional: keep running without a login session
```

Verify:

```sh
systemctl --user status orange-listener --no-pager
journalctl --user -u orange-listener -n 30 --no-pager
```

Within about 10 s the journal should show `connected: Air session open …`,
`catalog storage caught up`, and `room …: caught up, live` for each room. When a new message
arrives you should see `message: room=… sender=… -> firing webhook` followed by
`webhook POST ok status=200`. To test reconnect handling, run
`kill -USR1 $(systemctl --user show -p MainPID --value orange-listener)`. It simulates a dropped
session, and the listener reconnects without crashing.

Rollback:

```sh
systemctl --user disable --now orange-listener
rm ~/.config/systemd/user/orange-listener.service && systemctl --user daemon-reload
```

## client/ and sync.sh

`client/` is a scrubbed snapshot of the upstream `air-client` crate. To refresh it:

```sh
./sync.sh <PATH_TO_AIR_CLIENT_CHECKOUT>     # or set AIR_CLIENT_SRC; default ../air-client
```

`sync.sh` copies only source and text config: `Cargo.toml`, `Cargo.lock`, `src/**/*.rs`, and
`deploy/*.{service,example,template,sh}`. It never copies `target/`, `dist/`, binaries, tarballs,
`SHA256SUMS` or `*.env`. Upstream's top-level docs are not imported, because they describe a
private deployment. Their genericized public versions are hand-maintained and listed in
`client/.sync-keep`, and sync.sh never overwrites them. After copying, it rewrites
`/home/<user>/` paths and applies the rules from an optional local, gitignored `scrub.local` file
(`<regex><TAB><replacement>` per line, for example real room ids → `<ROOM_ID>`). Finally it refuses
to finish, listing `file:line` only, if anything that looks like a secret remains: long hex ids,
IPv4 addresses, private keys, GitHub tokens, real home paths, or a `scrub.local` pattern. It also
writes `client/STATUS.md`.

## License

MIT (see `LICENSE`). See `THIRD_PARTY_NOTICES.md` for the BSD-3-Clause notice that covers the
wire-compatible `ChatRoom` contract mirror (from ramp-stack/chk) and for dependency licenses.
