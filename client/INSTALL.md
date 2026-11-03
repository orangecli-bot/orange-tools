# air-client: install on a shared Linux server, one subject at a time

`air-client` replaces kiwi (`kiwi-serve` + `kiwi-daemon` + `kiwi call`) for one bot or person (a "subject") per Linux user.
It is a single binary that needs glibc 2.34 or newer and links only libc, libm and libgcc_s. Its subcommands are:
`listen` (run by the systemd unit), `send`, `read`, `unread`, `rooms`, `room create`, `whoami`, and `call` (a subset of kiwi's JSON API).
**Subjects keep their current Air name and their kiwi rooms.** This was tested with throwaway kiwi identities; see TEST-RESULTS.md.
Subjects that never used kiwi can follow the same steps and skip the migration parts.

> The `sudo systemctl --user -M user@`, `journalctl _UID=` and `ss` lines are standard systemd 248+ / iproute2 usage.
> If `-M` fails on your system, use the `XDG_RUNTIME_DIR` fallback described below.

Everything below runs as an **admin user with sudo** on the server. Set these per subject:
```sh
U=<SUBJECT_USER>                               # Linux user of the subject
H=/home/$U
KD=<KIWI_DATA_DIR>                             # the subject's kiwi data dir (holds state.json or secret.json)
D=$(sudo systemctl --user -M $U@ show kiwi-daemon -p WorkingDirectory --value)   # kiwi-daemon's dir
[ -n "$D" ] || D=<KIWI_DAEMON_DIR>             # fallback; must hold unread.jsonl / contacts.json / notify.env
echo "U=$U KD=$KD D=$D"; sudo ls -la $KD $D
```
If `systemctl -M` isn't available, use `sudo -u $U XDG_RUNTIME_DIR=/run/user/$(id -u $U) systemctl --user ...` everywhere it appears below.

## 0. Build or copy the binary and check it
Build `client/` reproducibly with `./build-release.sh` (run it inside `client/`). It runs `cargo build --release --locked`
with `--remap-path-prefix` for the crate dir, the cargo registry and CARGO_HOME, plus `CARGO_INCREMENTAL=0` and
`SOURCE_DATE_EPOCH=0`, and prints the sha256 of `target/release/air-client`. With the same toolchain (Rust 1.99.1) the
binary is byte-identical wherever it is built, so you can compare the hash with the release's `SHA256SUMS`.
Alternatively download `air-client-linux-x86_64` from a GitHub release. Copy the binary to `~/air-client-dist/`,
together with the files from `deploy/` (`air-client.service`, `air-client.env.template`, `notify.env.template`,
`air_send.sh`, `air_unread.sh`). If you ship it between machines, add a `SHA256SUMS` and check it on arrival:
```sh
cd ~/air-client-dist && sha256sum -c SHA256SUMS          # every line must say OK
./air-client --help | head -3
```

## 1. Make sure kiwi is stopped for this subject
```sh
sudo systemctl --user -M $U@ disable --now kiwi-daemon kiwi-serve
sudo systemctl --user -M $U@ is-active kiwi-daemon kiwi-serve     # both: inactive
sudo loginctl show-user $U -p Linger                              # Linger=yes (else: sudo loginctl enable-linger $U)
```
Record when kiwi stopped (used in step 3). **Don't rely on `systemctl show -p InactiveEnterTimestamp`**: on some
systems it comes back empty for user units, which used to leave `AIR_START_AFTER_NS` unset. Take the newest of these
signals instead; any one of them is enough:
```sh
# a) last write of kiwi's own state file (kiwi rewrites state.json on every change), epoch seconds
sudo stat -c %Y $KD/state.json
# b) last journal line of the kiwi units
sudo journalctl _UID=$(id -u $U) _SYSTEMD_USER_UNIT=kiwi-daemon.service _SYSTEMD_USER_UNIT=kiwi-serve.service -n 1 -o short-unix --no-pager | awk '{print int($1)}'
# c) last line of the daemon's own log (UTC ISO time at the start of each line)
sudo tail -n 1 $D/kiwi_daemon.log 2>/dev/null | awk '{print $1}' | xargs -r -I{} date -d {} +%s
# d) newest message kiwi delivered (ns -> s)
sudo cat $D/read.jsonl $D/unread.jsonl 2>/dev/null | jq -r '.timestamp_ns // empty' | sort -n | tail -1 | awk '{print int($1/1e9)}'
```
```sh
STOP_S=$( { sudo stat -c %Y $KD/state.json
            sudo journalctl _UID=$(id -u $U) _SYSTEMD_USER_UNIT=kiwi-daemon.service _SYSTEMD_USER_UNIT=kiwi-serve.service -n 1 -o short-unix --no-pager 2>/dev/null | awk 'NF{print int($1)}'
          } | grep -E '^[0-9]+$' | sort -n | tail -1 )
[ -n "$STOP_S" ] || STOP_S=$(date -d '<TIME_KIWI_STOPPED>' +%s)       # manual fallback if no signal is available
STOP_NS=$(( (STOP_S - 600) * 1000000000 ))                           # 10 min safety margin before the stop
date -d @$((STOP_NS/1000000000))                                      # sanity check: should be just before kiwi stopped
```
Why the margin is safe: messages kiwi already handled are skipped via `AIR_IMPORT_SEEN` (kiwi_seen.json ids), so the 10 minutes before the stop aren't delivered twice.
Without kiwi_seen.json, a message from those 10 minutes that kiwi had already delivered may show up once more in unread.jsonl.

## 2. Install the binary and directories (owned by the subject)
```sh
sudo install -d -o $U -g $U -m 700 $H/.config/air-client $H/.local/state/air-client
sudo install -d -o $U -g $U -m 755 $H/.local/bin $H/.config/systemd/user
sudo install -o $U -g $U -m 755 ~/air-client-dist/air-client $H/.local/bin/air-client
```

## 3. Identity and config. Copy files only; never print or paste a secret.
Copy the kiwi identity into a mode-600 file. The secret goes file to file and never reaches the terminal:
```sh
if sudo test -f $KD/state.json; then
  sudo -u $U sh -c "umask 077; jq -c .secret '$KD/state.json' > '$H/.config/air-client/identity.json'"
else
  sudo -u $U sh -c "umask 077; cp '$KD/secret.json' '$H/.config/air-client/identity.json'"
fi
sudo ls -l $H/.config/air-client/identity.json          # -rw------- $U
```
(You can also point AIR_IDENTITY_FILE straight at `$KD/state.json`. A separate copy keeps air-client independent of kiwi's files.)

Write the config from the template. It holds paths only, no secrets:
```sh
# STOP_NS from step 1
sed -e "s#/home/__USER__#$H#g" -e "s#__DIR__#$D#g" -e "s#^AIR_START_AFTER_NS=.*#AIR_START_AFTER_NS=$STOP_NS#" \
    ~/air-client-dist/air-client.env.template > /tmp/air-client.env.$U
sudo install -o $U -g $U -m 600 /tmp/air-client.env.$U $H/.config/air-client/air-client.env && rm /tmp/air-client.env.$U
```
Then adjust per subject:
* **No wake:** comment out `AIR_NOTIFY_ENV=`. A notify.env that doesn't exist also means no wake. See `docs/no-wake-subject.md`.
* **With wake:** the file at `AIR_NOTIFY_ENV=$D/notify.env` must exist and be **mode 600** (`sudo chmod 600 $D/notify.env`), otherwise the listener refuses to start. `deploy/notify.env.template` shows its format.
* If `$D/kiwi_seen.json` doesn't exist, comment out `AIR_IMPORT_SEEN=`. `AIR_START_AFTER_NS` alone is then the cutoff.
* Check that `AIR_START_AFTER_NS` is a number (`grep START_AFTER $H/.config/air-client/air-client.env` as root). If it's empty, step 1 found no signal: set `STOP_S` by hand and re-run the `sed`.
* If the subject's own tools only get write access to one workspace directory, put `AIR_STATE_DIR`, `AIR_UNREAD_FILE` and `AIR_READ_FILE`
  under that directory, for example `<WORKSPACE>/air-client-state`, `<WORKSPACE>/unread.jsonl` and `<WORKSPACE>/read.jsonl`.
```sh
sudo chmod 600 $H/.config/air-client/air-client.env; sudo cat $H/.config/air-client/air-client.env   # paths only, safe to view
```

## 4. Check the identity BEFORE starting anything
```sh
AC="sudo -u $U $H/.local/bin/air-client --config $H/.config/air-client/air-client.env"
$AC whoami                                  # must print the subject's current Air name (the kiwi-era one)
sudo cat $D/name.txt; echo                  # kiwi-era name.txt, if the subject has one
test "$($AC whoami)" = "$(sudo cat $D/name.txt)" && echo NAME-MATCH
```

## 5. Install and start the listener unit
```sh
sudo install -o $U -g $U -m 644 ~/air-client-dist/air-client.service $H/.config/systemd/user/air-client.service
sudo systemctl --user -M $U@ daemon-reload
sudo systemctl --user -M $U@ enable --now air-client
sudo journalctl _UID=$(id -u $U) _SYSTEMD_USER_UNIT=air-client.service -n 40 --no-pager
```
Within about 10 s the journal should show `air-client listen starting for <name>; ... wake on (Kiwi format, batch 1500 ms)` (or `wake off`),
then `connected: Air session open ...`, `catalog storage caught up`, and one `room <id>: following` plus `caught up, live` line per room.
The first start replays each room's history once, so big rooms take longer. Messages that arrived after `AIR_START_AFTER_NS`
and aren't in kiwi_seen.json are appended to unread.jsonl, and a wake fires if wake is configured.

## 6. Install the shims (drop-in replacements for the kiwi-era scripts)
If clients call `$D/air_unread.sh` and `$D/air_send.sh` (for example over ssh), replacing those files switches them over with no client-side change:
```sh
sudo cp -a $D/air_unread.sh $D/air_unread.sh.kiwi-bak 2>/dev/null
sudo install -o $U -g $U -m 700 ~/air-client-dist/air_unread.sh $D/air_unread.sh
sudo install -o $U -g $U -m 700 ~/air-client-dist/air_send.sh   $D/air_send.sh
```
Clients that ran `kiwi --data-dir <KIWI_DATA_DIR> call -` remotely change **only that remote command** to
`/home/<SUBJECT_USER>/.local/bin/air-client --config /home/<SUBJECT_USER>/.config/air-client/air-client.env call -`.
The stdin JSON (`{op:"send",id,text,request_id}`) and the `{"ok":true,"result":{...}}` reply stay the same.

## 7. Verify (do all of it for each subject)
```sh
# a) exactly one long-lived socket for the subject's listener
sudo ss -tnp | grep air-client                      # one ESTAB to :443 per running subject (pid = MainPID below)
sudo systemctl --user -M $U@ show air-client -p MainPID -p ActiveState -p NRestarts
# b) name kept
test "$($AC whoami)" = "$(sudo cat $D/name.txt)" && echo NAME-MATCH
# c) rooms visible (the kiwi rooms, same ids as before)
$AC rooms --members
# d) read path
$AC read <ROOM_ID> -n 5                             # last 5 text messages of a room
$AC unread                                          # prints JSON lines or "no new messages"; marks them read
# e) send path: use a test room shared only between this subject and an identity you control (find it with `rooms --members`)
$AC send <ROOM_ID> "air-client migration check for $U - no reply needed"
#    expect {"ok":true,"result":{"id":"<ROOM_ID>","status":"sent",...}}
# f) inbound + wake: send a line into that room from the other identity; within a second or two the subject's journal shows
#    "message: room=... -> unread.jsonl" and, for wake subjects, "wake POST -> 200 ... (1 message(s))".
sudo journalctl _UID=$(id -u $U) _SYSTEMD_USER_UNIT=air-client.service -n 10 --no-pager
# g) reconnect self-test (optional): forces the closed-session path; it must reconnect without restarting
sudo kill -USR1 $(sudo systemctl --user -M $U@ show air-client -p MainPID --value)
```
`send` refuses a room that isn't in the subject's room list, and one where the subject isn't a decoded member
(`{"ok":false,"error":...}`, exit 2). `--force` skips only the member check (for Orange group rooms).

## 7b. Create a room (for subjects that share no room)
`room create --member <AIR_NAME>` creates a room with the given members and prints its id. For a 1:1 DM it first looks for an
existing DM with exactly {subject, member} and returns that one (`existing`). Otherwise it creates the room the way Orange 1.0.1
and kiwi do: init = hash of the recipient, `Share(recipient)`, registered in the creator's storage, and delivered to the
recipient's inbox with a notification. **No restart is needed** on either side: the creator's and the recipient's running
listeners pick up the new room live (tested: about 1 s). The recipient doesn't need to be online; its listener finds the room
the next time it connects.
```sh
NAME_OF() { sudo -u $1 /home/$1/.local/bin/air-client --config /home/$1/.config/air-client/air-client.env whoami; }
A=<SUBJECT_USER_A>; B=<SUBJECT_USER_B>
R=$(sudo -u $A /home/$A/.local/bin/air-client --config /home/$A/.config/air-client/air-client.env room create --member "$(NAME_OF $B)")
echo "$A<->$B room: $R"
```
Then prove it works both ways, using the printed id as `R`:
```sh
sudo -u $A /home/$A/.local/bin/air-client --config /home/$A/.config/air-client/air-client.env send $R "DM check $A -> $B, no reply needed"
sudo -u $B /home/$B/.local/bin/air-client --config /home/$B/.config/air-client/air-client.env unread      # shows it
sudo -u $B /home/$B/.local/bin/air-client --config /home/$B/.config/air-client/air-client.env send $R "DM check $B -> $A"
sudo -u $A /home/$A/.local/bin/air-client --config /home/$A/.config/air-client/air-client.env unread
```
- **Idempotent:** running `room create` again returns the same id with status `existing`, so it's safe to repeat. The other side
  running `room create --member <creator's AIR_NAME>` also finds the same DM.
- **kiwi-compatible JSON:** `air-client call '{"op":"room","members":["<AIR_NAME>"],"request_id":"<STABLE_ID>"}'` returns
  `{"ok":true,"result":{"id":"<ROOM_ID>","status":"created"|"existing"}}`. Repeating it with the same `request_id` returns the stored
  result and creates nothing new.
- `--json` makes `room create` print the same `{"ok":true,"result":{...}}` shape.
- **Groups:** with two or more `--member` flags, the room uses kiwi's group form (random init, one `Share` per member). Orange shows
  these rooms with 3+ members, but they aren't Orange "named groups".
- Every CLI command makes up to 3 connection attempts, so a single websocket handshake timeout doesn't fail `room create`.

## 8. Rollback (per subject)
```sh
sudo systemctl --user -M $U@ disable --now air-client
sudo cp -a $D/air_unread.sh.kiwi-bak $D/air_unread.sh 2>/dev/null
sudo systemctl --user -M $U@ enable --now kiwi-serve kiwi-daemon
```
Revert any client-side remote command to `kiwi --data-dir <KIWI_DATA_DIR> call -`.
air-client never writes to kiwi-data, so kiwi resumes from its own state. Messages that air-client already put in unread.jsonl stay there.
After a rollback, kiwi may re-deliver messages air-client already handled (kiwi_seen.json doesn't know about them), so expect a few duplicates.
To remove air-client completely: `sudo rm -rf $H/.local/state/air-client` and `sudo shred -u $H/.config/air-client/identity.json`.

## What the listener writes and sends
* **unread.jsonl line** (same keys and order as kiwi_daemon.sh): `{"from":"<contacts.json label|unknown>","sender":"<name>","room":"<room id>","timestamp_ns":N,"content":"<text>"}`.
  One line per new **text** message from someone other than the subject. Content that contains the subject's private key is replaced by `<redacted: contained private key material>` and never triggers a wake.
* **Wake POST** (only if AIR_NOTIFY_ENV is set and has both values): `POST $AIR_WAKE_URL` with headers `Authorization: Bearer $AIR_WAKE_KEY` and `Content-Type: application/json`. Body:
  `{"event":"air_message","count":N,"messages":[{"sender":"<name>","room":"<room id>","timestamp":<ns>}, ...]}`. That's kiwi_daemon.sh's exact shape: metadata only, no content.
  Messages that arrive within `AIR_WAKE_BATCH_MS` (default 1500 ms) of the first one share one POST. **One attempt per batch: no retries, no re-wake, no auto-ack, no receipts.**
  A failed POST is only logged (`wake POST failed ...`); the message is still in unread.jsonl.
  `AIR_WAKE_FORMAT=orange` sends one `{"event":"orange_message","source":"air-client","me","sender","room","content"(≤2000 chars),"ts","ts_ns"}` per message instead (orange-listener's format).
* **State**: `$AIR_STATE_DIR/listen/{catalog.bin,listen-state.json,rooms/*.instance,rooms/*.jsonl}` and `$AIR_STATE_DIR/cli/...` (CLI cache, send receipts). Everything is mode 600 under 700 dirs. The catalog and instances embed derived keys, so treat the state dir as secret.

## Known limits
* Only **text** messages are decoded. Stickers, GIFs, photos, video, deletes and Orange group ops are skipped: no unread line, no wake. (kiwi wrote `<Sticker>`-style placeholders.) The cursor still advances, so nothing stalls.
* `call` supports `identity`, `list`, `get`, `messages`, `send` and `room`. It does not support `events`, `watch`, `contact`, `share`, `block`, `delete` or `send-video`.
* `send` returns `"status":"sent"` once the server confirms the write (kiwi returned `"queued"` immediately), plus `message_id`, `timestamp_ns` and timings.
  request_id idempotency is tracked in air-client's own receipts; kiwi-era receipts aren't imported.
* The first time the CLI touches a room that has never been opened, it costs one network round trip per stored message (about 0.2 s each). With the listener running, the CLI starts from the listener's caught-up copy instead (measured: 2 round trips, 0.8 s).
* The listener and the CLI each open their own connection. The listener's is the one long-lived socket; CLI connections close when the command exits.
