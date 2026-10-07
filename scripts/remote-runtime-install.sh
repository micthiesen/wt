#!/usr/bin/env bash
set -euo pipefail

payload=$HOME/$1
hash=$2
checksum=$3
metadata=$4
root=$HOME/.cache/wt/runtimes
target=$root/$hash
stage=$(mktemp -d "$root/.stage.XXXXXX")
cleanup() {
  rm -f "$payload"
  rm -rf "$stage"
}
trap cleanup EXIT

# Concurrent controllers can prepare one package. Rename only after validation.
if [[ -f "$target/.ready" ]]; then exit 0; fi
actual=$(sha256sum "$payload" | cut -d ' ' -f 1)
[[ "$actual" == "$checksum" ]] || { echo 'wt: runtime archive checksum mismatch' >&2; exit 1; }
tar -xzpf "$payload" -C "$stage"
printf '%s' "$metadata" | base64 -d > "$stage/.wt-runtime.json"
bun_bin=$(command -v bun || true)
if [[ -z "$bun_bin" && -x "$HOME/.bun/bin/bun" ]]; then bun_bin=$HOME/.bun/bin/bun; fi
[[ -n "$bun_bin" ]] || { echo 'wt: install Bun on the remote host first' >&2; exit 1; }
(cd "$stage" && "$bun_bin" install --frozen-lockfile >&2)
handshake=$(cd "$stage" && WT_UPDATE=off WT_SKILLS=off "$bun_bin" src/main.ts _hello)
WT_RUNTIME_HANDSHAKE="$handshake" WT_RUNTIME_HASH="$hash" "$bun_bin" -e '
  const info = JSON.parse(process.env.WT_RUNTIME_HANDSHAKE ?? "{}");
  const meta = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  if (info.role !== "worker" || info.protocol !== meta.protocol || info.build !== meta.build || meta.hash !== process.env.WT_RUNTIME_HASH) {
    throw new Error("wt: runtime handshake failed; remote config must set [instance] role = worker");
  }
' "$stage/.wt-runtime.json"
touch "$stage/.ready"
# flock closes on exit; it does not leave a stale ownership directory.
exec 9>"$root/.install.lock"
flock 9
if [[ ! -f "$target/.ready" ]]; then mv -T "$stage" "$target"; fi
