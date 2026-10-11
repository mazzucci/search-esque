#!/bin/sh
# Puts the official SQLite WebAssembly build (FTS5 and JSON included) under web/vendor/sqlite,
# fetched from the npm registry's tarball so the page depends on no CDN at run time. Pinned.
set -eu
VERSION=3.53.0-build1
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
curl -sSL -o "$tmp/pkg.tgz" "https://registry.npmjs.org/@sqlite.org/sqlite-wasm/-/sqlite-wasm-$VERSION.tgz"
tar xzf "$tmp/pkg.tgz" -C "$tmp"
mkdir -p web/vendor/sqlite
cp "$tmp/package/dist/index.mjs" "$tmp/package/dist/sqlite3.wasm" "$tmp/package/dist/sqlite3-opfs-async-proxy.js" web/vendor/sqlite/
printf 'SQLite WebAssembly %s from the npm package @sqlite.org/sqlite-wasm (Apache-2.0; SQLite itself is public domain).\nFetched by dev/vendor_sqlite.sh; not part of this repository.\n' "$VERSION" > web/vendor/sqlite/NOTICE
rm -rf "$tmp"
echo "web/vendor/sqlite: SQLite WebAssembly $VERSION"
