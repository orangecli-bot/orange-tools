#!/usr/bin/env bash
# sync.sh - refresh client/ from a working copy of the air-client crate, scrubbed for publication.
#
#   ./sync.sh [SOURCE_DIR]        (default: $AIR_CLIENT_SRC, else ../air-client)
#
# Copies ONLY source and text config: Cargo.toml, Cargo.lock, build-release.sh, src/**/*.rs, and deploy/ text files
# (*.service, *.example, *.template, *.sh shims). Never copies target/, dist/, tarballs, SHA256SUMS,
# binaries, *.env, identity/state files, keys or tokens.
#
# Hand-maintained files: upstream's top-level docs (README, install guide, per-subject notes, test
# results) name real hosts and users, so they are NOT copied. Their genericized public versions live
# in client/ and are listed in client/.sync-keep (one path per line, relative to client/, a trailing
# / keeps a whole directory). Kept paths are preserved as-is across syncs; upstream docs that are
# not imported are listed in client/STATUS.md so they can be reviewed and re-genericized by hand.
#
# Scrubbing:
#   * generic rewrites: /home/<user>/ -> $HOME/ ;  /workspace/ -> ./
#   * optional local denylist `scrub.local` (gitignored, never committed). One rule per line:
#         <regex>\t<replacement>      e.g.  some-real-room-id<TAB><ROOM_ID>
#     Lines starting with # are ignored. Use it for real names, keys, room ids, hostnames.
#   * afterwards a generic leak check runs over client/ and the script exits non-zero (listing
#     file:line only, never the matched text) if anything suspicious remains:
#     hex strings >= 40 chars, IPv4 addresses, private-key headers, GitHub tokens, /home/ paths,
#     and every regex from scrub.local.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
src="${1:-${AIR_CLIENT_SRC:-$here/../air-client}}"
dst="$here/client"
[ -f "$src/Cargo.toml" ] || { echo "sync.sh: $src has no Cargo.toml" >&2; exit 1; }

rm -rf "$dst.tmp"; mkdir -p "$dst.tmp"
copy() { local rel="$1"; mkdir -p "$dst.tmp/$(dirname "$rel")"; cp "$src/$rel" "$dst.tmp/$rel"; }
keep=()
[ -f "$dst/.sync-keep" ] && while read -r k; do [[ -z "$k" || "$k" == \#* ]] || keep+=("$k"); done < "$dst/.sync-keep"
is_kept() { local k; for k in "${keep[@]}"; do [[ "$1" == "$k" || ( "$k" == */ && "$1" == "$k"* ) ]] && return 0; done; return 1; }
for f in Cargo.toml Cargo.lock build-release.sh; do [ -f "$src/$f" ] && ! is_kept "$f" && copy "$f"; done
( cd "$src" && find src -type f -name '*.rs' 2>/dev/null; \
  find deploy -type f \( -name '*.service' -o -name '*.example' -o -name '*.template' -o -name '*.sh' \) 2>/dev/null ) \
  | sort -u | while read -r rel; do is_kept "$rel" || copy "$rel"; done

# generic rewrites
find "$dst.tmp" -type f -print0 | xargs -0 -r sed -i -E \
  -e 's#/home/[A-Za-z0-9._-]+/#$HOME/#g' -e 's#/workspace/#./#g'
# local denylist rewrites
if [ -f "$here/scrub.local" ]; then
  while IFS=$'\t' read -r re rep; do
    [[ -z "$re" || "$re" == \#* ]] && continue
    find "$dst.tmp" -type f -print0 | xargs -0 -r perl -pi -e "s{$re}{$rep}gi"
  done < "$here/scrub.local"
fi

# restore hand-maintained files from the current client/
for k in "${keep[@]}"; do
  if [ -e "$dst/$k" ]; then mkdir -p "$dst.tmp/$(dirname "${k%/}")"; cp -a "$dst/${k%/}" "$dst.tmp/$(dirname "${k%/}")/"; fi
done
[ -f "$dst/.sync-keep" ] && cp "$dst/.sync-keep" "$dst.tmp/.sync-keep"

# status note (always written): snapshot time and anything upstream has not produced yet
missing=()
[ -f "$dst.tmp/src/main.rs" ] || missing+=("src/main.rs (binary entry point)")
[ -f "$dst.tmp/Cargo.lock" ] || missing+=("Cargo.lock")
[ -n "$(ls -A "$src/deploy" 2>/dev/null)" ] || missing+=("deploy/ (systemd unit and config templates; upstream directory is empty)")
notimported=$(cd "$src" && find . -maxdepth 1 -type f -name '*.md' -printf '%P\n' | sort)
for doc in $(grep -rhoE '\b[A-Za-z0-9_-]+\.md\b' "$dst.tmp/src" 2>/dev/null | sort -u); do
  [ -f "$dst.tmp/$doc" ] || missing+=("$doc (referenced in the source, not present upstream)")
done
{ echo "# client/ status"; echo; echo "Snapshot of the upstream air-client working copy, synced $(date -u +%Y-%m-%dT%H:%MZ) by ./sync.sh."; \
  echo "Re-run ./sync.sh to refresh from upstream."; echo; \
  if [ ${#missing[@]} -gt 0 ]; then echo "Not yet available upstream (left out):"; for m in "${missing[@]}"; do echo "- $m"; done; \
  else echo "Nothing known missing."; fi; \
  if [ -n "$notimported" ]; then echo; echo "$(echo "$notimported" | wc -w) upstream top-level doc(s) were not imported verbatim; the public, genericized"; \
    echo "versions are hand-maintained and listed in .sync-keep (README.md, INSTALL.md, TEST-RESULTS.md, docs/)."; \
    echo "Review upstream doc changes by hand after each sync."; fi; } > "$dst.tmp/STATUS.md"

# scrub the status note too (it lists upstream file names)
if [ -f "$here/scrub.local" ]; then
  while IFS=$'\t' read -r re rep; do [[ -z "$re" || "$re" == \#* ]] && continue
    perl -pi -e "s{$re}{$rep}gi" "$dst.tmp/STATUS.md"; done < "$here/scrub.local"
fi

# leak check (prints file:line only)
# generic patterns (Cargo.lock is exempt from the hex/IP rule: it is full of crate checksums)
gen='\b[0-9a-fA-F]{40,}\b|\b([0-9]{1,3}\.){3}[0-9]{1,3}\b'
always='BEGIN [A-Z ]*PRIVATE|ghp_[A-Za-z0-9]|github_pat_|/home/[A-Za-z0-9._-]*'
if [ -f "$here/scrub.local" ]; then
  while IFS=$'\t' read -r re rep; do [[ -z "$re" || "$re" == \#* ]] && continue; always="$always|$re"; done < "$here/scrub.local"
fi
hits=$( { grep -rnIiE -o --exclude=Cargo.lock "$gen" "$dst.tmp" || true; grep -rnIiE -o "$always" "$dst.tmp" || true; } \
        | grep -vE ':(127\.0\.0\.1|0\.0\.0\.0|/home/|/home/__USER__)$' | cut -d: -f1,2 | sort -u || true)
if [ -n "$hits" ]; then
  echo "sync.sh: possible secrets/identifiers remain (file:line); fix scrub.local and re-run:" >&2
  echo "$hits" | sed "s#$dst.tmp/#client/#" >&2
  rm -rf "$dst.tmp"; exit 2
fi
rm -rf "$dst"; mv "$dst.tmp" "$dst"
echo "sync.sh: client/ refreshed from $src"
[ -f "$dst/STATUS.md" ] && cat "$dst/STATUS.md"
exit 0
