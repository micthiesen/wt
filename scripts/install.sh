#!/bin/sh
set -eu

usage() {
  cat <<'HELP'
Install native wt from a verified GitHub release.

Usage: install.sh [--channel stable|preview] [--release TAG] [--root PATH] [--path]

The default installs the latest stable release into ~/.local/share/wt. --release
selects one exact release tag, including rust-test-* artifacts. --path creates
~/.local/bin/wt when that path is free or migrates a recognized legacy wt link.
HELP
}

channel=stable
release_tag=
install_root=${WT_INSTALL_ROOT:-${HOME:?HOME must be set}/.local/share/wt}
path_link=0
explicit_channel=0
while [ "$#" -gt 0 ]; do
  case $1 in
    --help|-h) usage; exit 0 ;;
    --channel)
      [ "$#" -ge 2 ] || { echo 'wt install: --channel needs stable or preview' >&2; exit 2; }
      channel=$2; explicit_channel=1; shift 2 ;;
    --release)
      [ "$#" -ge 2 ] || { echo 'wt install: --release needs a tag' >&2; exit 2; }
      release_tag=$2; shift 2 ;;
    --root)
      [ "$#" -ge 2 ] || { echo 'wt install: --root needs a path' >&2; exit 2; }
      install_root=$2; shift 2 ;;
    --path) path_link=1; shift ;;
    *) echo "wt install: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case $channel in stable|preview) ;; *) echo 'wt install: channel must be stable or preview' >&2; exit 2 ;; esac
if [ -n "$release_tag" ] && [ "$explicit_channel" -eq 1 ]; then
  echo 'wt install: --release and --channel cannot be combined' >&2
  exit 2
fi
if [ -n "$release_tag" ]; then
  case $release_tag in ''|[!A-Za-z0-9]*|*[!A-Za-z0-9._+-]*) echo 'wt install: invalid release tag' >&2; exit 2 ;; esac
fi
case $install_root in /*) ;; *) echo 'wt install: root must be an absolute path' >&2; exit 2 ;; esac

repo=${WT_RELEASE_REPOSITORY:-micthiesen/wt}
case $repo in
  */*) owner=${repo%%/*}; name=${repo#*/} ;;
  *) echo 'wt install: WT_RELEASE_REPOSITORY must be owner/name' >&2; exit 2 ;;
esac
case $owner in ''|*[!A-Za-z0-9._+-]*) echo 'wt install: invalid repository owner' >&2; exit 2 ;; esac
case $name in ''|*[!A-Za-z0-9._+-]*|*/*) echo 'wt install: invalid repository name' >&2; exit 2 ;; esac

case $(uname -s) in
  Darwin)
    case $(uname -m) in
      arm64|aarch64) target=aarch64-apple-darwin ;;
      x86_64|amd64) target=x86_64-apple-darwin ;;
      *) echo "wt install: unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
    esac ;;
  Linux)
    case $(uname -m) in
      aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
      x86_64|amd64) target=x86_64-unknown-linux-gnu ;;
      *) echo "wt install: unsupported Linux architecture: $(uname -m)" >&2; exit 1 ;;
    esac ;;
  *) echo "wt install: unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/wt-install.XXXXXX") || exit 1
cleanup() { rm -rf "$tmp"; }
trap cleanup 0 HUP INT TERM

if [ -n "$release_tag" ]; then
  bootstrap_tag=$release_tag
  sums_url="https://github.com/$repo/releases/download/$bootstrap_tag/SHA256SUMS"
else
  # Preview uses a stable binary as the small bootstrap; the native client
  # selects and verifies the preview channel through GitHub's API.
  sums_url="https://github.com/$repo/releases/latest/download/SHA256SUMS"
fi
sums=$tmp/SHA256SUMS
curl --fail --location --silent --show-error --retry 2 \
  --user-agent 'OpenAI File Downloader, XaiImageApiFetch/1.0' \
  --output "$sums" "$sums_url"

suffix="-$target.tar.gz"
asset_line=$(awk -v suffix="$suffix" '
  NF == 2 && length($1) == 64 && $1 ~ /^[0-9A-Fa-f]+$/ &&
  index($2, "wt-") == 1 && substr($2, length($2)-length(suffix)+1) == suffix {
    count++
    digest=$1
    filename=$2
  }
  END { if (count != 1) exit 1; print digest " " filename }
' "$sums") || { echo "wt install: no unique checksum entry for $target" >&2; exit 1; }
expected_hash=$(printf '%s' "$asset_line" | cut -d ' ' -f 1)
archive_name=$(printf '%s' "$asset_line" | cut -d ' ' -f 2)
case $archive_name in *[!A-Za-z0-9._+-]*) echo 'wt install: unsafe asset name in SHA256SUMS' >&2; exit 1 ;; esac
stem=${archive_name%.tar.gz}
encoded_tag=${stem#wt-}
bootstrap_tag=${encoded_tag%-$target}
case $bootstrap_tag in ''|[!A-Za-z0-9]*|*[!A-Za-z0-9._+-]*) echo 'wt install: unsafe release tag in SHA256SUMS' >&2; exit 1 ;; esac
if [ -n "$release_tag" ] && [ "$bootstrap_tag" != "$release_tag" ]; then
  echo 'wt install: checksum asset did not match the requested release tag' >&2
  exit 1
fi
archive_url="https://github.com/$repo/releases/download/$bootstrap_tag/$archive_name"
archive=$tmp/$archive_name
curl --fail --location --silent --show-error --retry 2 \
  --user-agent 'OpenAI File Downloader, XaiImageApiFetch/1.0' \
  --output "$archive" "$archive_url"

if command -v sha256sum >/dev/null 2>&1; then
  actual_hash=$(sha256sum "$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual_hash=$(shasum -a 256 "$archive" | awk '{print $1}')
else
  echo 'wt install: sha256sum or shasum is required to verify the release' >&2
  exit 1
fi
expected_lower=$(printf '%s' "$expected_hash" | tr 'A-F' 'a-f')
[ "$actual_hash" = "$expected_lower" ] || {
  echo 'wt install: archive checksum mismatch' >&2
  exit 1
}

members=$tmp/members
tar -tzf "$archive" > "$members" || { echo 'wt install: release archive is unreadable' >&2; exit 1; }
seen_wt=0
seen_launcher=0
seen_build_info=0
while IFS= read -r member; do
  case $member in
    "$stem/wt") [ "$seen_wt" -eq 0 ] || { echo 'wt install: duplicate application member' >&2; exit 1; }; seen_wt=1 ;;
    "$stem/wt-launcher") [ "$seen_launcher" -eq 0 ] || { echo 'wt install: duplicate launcher member' >&2; exit 1; }; seen_launcher=1 ;;
    "$stem/wt-build-info.json") [ "$seen_build_info" -eq 0 ] || { echo 'wt install: duplicate build-info member' >&2; exit 1; }; seen_build_info=1 ;;
    *) echo "wt install: unexpected archive entry: $member" >&2; exit 1 ;;
  esac
done < "$members"
[ "$seen_wt" -eq 1 ] && [ "$seen_launcher" -eq 1 ] && [ "$seen_build_info" -eq 1 ] || {
  echo 'wt install: archive is missing required release files' >&2
  exit 1
}
tar -xzf "$archive" -C "$tmp" "$stem/wt"
bootstrap=$tmp/$stem/wt
[ -f "$bootstrap" ] && [ ! -L "$bootstrap" ] || { echo 'wt install: bootstrap application is not a regular file' >&2; exit 1; }
chmod 755 "$bootstrap"
probe=$("$bootstrap" --_boot-probe) || { echo 'wt install: bootstrap application failed its identity probe' >&2; exit 1; }
case $probe in wt-build-id:*:"$target") ;; *) echo "wt install: bootstrap target mismatch: $probe" >&2; exit 1 ;; esac
build_id=${probe#wt-build-id:}
build_id=${build_id%:$target}
case $build_id in *[!0-9a-fA-F]*) echo 'wt install: bootstrap returned an invalid build id' >&2; exit 1 ;; esac
case ${#build_id} in 40|64) ;; *) echo 'wt install: bootstrap returned an incomplete build id' >&2; exit 1 ;; esac

set -- install
if [ "$channel" = preview ] && [ -z "$release_tag" ]; then
  set -- "$@" --channel preview
elif [ "$channel" = stable ] && [ -z "$release_tag" ]; then
  # Pin the native installer to the exact stable artifact we just verified.
  # A moving latest endpoint between bootstrap and metadata fetch must not
  # silently select a different build.
  set -- "$@" --release "$bootstrap_tag" --channel stable --expected-build-id "$build_id"
else
  set -- "$@" --release "$bootstrap_tag" --expected-build-id "$build_id"
fi
[ "$path_link" -eq 0 ] || set -- "$@" --path
WT_INSTALL_ROOT=$install_root WT_RELEASE_REPOSITORY=$repo \
  "$bootstrap" "$@"
