#!/bin/sh
# Two machines with the bothy CLI: this host and a Linux container.
#
#   ./run.sh path     shared folder remote, fully automatic (no Google account)
#   ./run.sh drive    Google Drive folder remote; signs in on both sides
#
# Needs Docker. Run from anywhere; paths are relative to this script.
set -eu

mode=${1:-path}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work="$here/.work"
image=rust:1-bookworm

mkdir -p "$work"
echo "== building bothy for this host"
cargo build --quiet --release -p bothy-cli --manifest-path "$repo/Cargo.toml"
host_bothy="$repo/target/release/bothy"

echo "== building bothy for Linux (in Docker; the first run takes a while)"
docker run --rm -v "$repo:/src" -v bothy-cargo-registry:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/src/target/docker -w /src "$image" \
  cargo build --quiet --release -p bothy-cli

a() { BOTHY_HOME="$work/a" BOTHY_TOKEN_STORE=file "$host_bothy" "$@"; }
b() {
  docker run --rm -i -v "$repo:/src" -v "$work:/work" -e BOTHY_HOME=/work/b \
    -e BOTHY_TOKEN_STORE=file "$image" /src/target/docker/release/bothy "$@"
}

rm -rf "$work/a" "$work/b" "$work/shared"
case "$mode" in
  path)
    a init --remote "path:$work/shared"
    b join --remote path:/work/shared
    ;;
  drive)
    : "${DESKTOP_CLIENT:?set DESKTOP_CLIENT to your Desktop app client JSON}"
    : "${DEVICE_CLIENT:?set DEVICE_CLIENT to your TVs and Limited Input client JSON}"
    cp "$DEVICE_CLIENT" "$work/device-client.json"
    a auth setup --client "$DESKTOP_CLIENT"
    a auth login
    b auth setup --device-client /work/device-client.json
    b auth login --device
    a init --remote folder:cli-two-machines
    b join --remote folder:cli-two-machines
    ;;
  *) echo "usage: $0 [path|drive]" >&2; exit 2 ;;
esac

echo "== both machines write while offline"
a put notes n1 '{"title":"written on the host"}'
b put notes n1 '{"tags":["linux"]}' --patch
b put todos t1 '{"done":false}'

echo "== sync both ways"
a sync
b sync
a sync

a export --all > "$work/a.jsonl"
b export --all > "$work/b.jsonl"
if cmp -s "$work/a.jsonl" "$work/b.jsonl"; then
  echo "== OK: export --all is identical on both machines"
  cat "$work/a.jsonl"
else
  echo "== MISMATCH" >&2
  diff "$work/a.jsonl" "$work/b.jsonl" >&2
  exit 1
fi
