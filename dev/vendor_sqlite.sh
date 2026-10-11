#!/bin/sh
# Puts the official SQLite WebAssembly build (FTS5 and JSON included) under web/vendor/sqlite,
# fetched from the npm registry's tarball so the page depends on no CDN at run time. Pinned.
set -eu
VERSION=3.53.0-build1
SHA256=fbbd5c542fad22ce1d90df1533025903a00a593f461f4a43b1e43e9897dfad9c
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
curl -sSL -o "$tmp/pkg.tgz" "https://registry.npmjs.org/@sqlite.org/sqlite-wasm/-/sqlite-wasm-$VERSION.tgz"
echo "$SHA256  $tmp/pkg.tgz" | shasum -a 256 -c - >/dev/null
tar xzf "$tmp/pkg.tgz" -C "$tmp"
curl -sSL -o "$tmp/LICENSE-Apache-2.0" https://www.apache.org/licenses/LICENSE-2.0.txt
mkdir -p web/vendor/sqlite
cp "$tmp/package/dist/index.mjs" "$tmp/package/dist/sqlite3.wasm" "$tmp/package/dist/sqlite3-opfs-async-proxy.js" web/vendor/sqlite/
cp "$tmp/LICENSE-Apache-2.0" web/vendor/sqlite/LICENSE-Apache-2.0
cat > web/vendor/sqlite/NOTICE <<EOF
SQLite WebAssembly $VERSION, from the npm package @sqlite.org/sqlite-wasm.
The npm packaging is licensed under Apache-2.0 (LICENSE-Apache-2.0 beside this file). The bundle's
own header says the Emscripten glue is under the MIT and NCSA licences, and SQLite itself is in the
public domain. Fetched by dev/vendor_sqlite.sh; these files are not part of the search-esque
repository and are redistributed unchanged.
EOF
rm -rf "$tmp"
echo "web/vendor/sqlite: SQLite WebAssembly $VERSION"
