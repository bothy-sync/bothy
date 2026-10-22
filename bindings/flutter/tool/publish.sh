#!/bin/sh
# Publishes the package to pub.dev: `tool/publish.sh` (or `tool/publish.sh --dry-run`).
#
# In the repository the native crate depends on the core by path. A published package has
# no such path, so for the upload rust/Cargo.toml points at the crates.io release with the
# same version as the package; publish `bothy` to crates.io first.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version: //p' pubspec.yaml)
path_dep='bothy = { path = "../../../crates/bothy" }'
grep -qF "$path_dep" rust/Cargo.toml || { echo "rust/Cargo.toml: path dependency not found" >&2; exit 1; }
cp rust/Cargo.toml rust/Cargo.toml.orig
trap 'mv rust/Cargo.toml.orig rust/Cargo.toml' EXIT INT TERM
sed "s|^bothy = { path = \"../../../crates/bothy\" }|bothy = \"=$version\"|" rust/Cargo.toml.orig > rust/Cargo.toml
grep -q "^bothy = \"=$version\"" rust/Cargo.toml
flutter pub publish "$@"
