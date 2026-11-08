#!/bin/bash
# Copyright (c) 2026 Adequate Systems HK Limited
# Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.
# Writes THIRD-PARTY-NOTICES.txt from Cargo.lock, or checks the committed copy
# against freshly generated output.
#
#   bash ci/notices.sh            regenerate THIRD-PARTY-NOTICES.txt
#   bash ci/notices.sh --check    fail, with a diff, if the committed file
#                                 differs (run by CI)
#
# The generator is cargo-about 0.9.2, installed with
#   cargo install --locked --features cli cargo-about --version 0.9.2
# Without `--features cli` that command installs nothing and still exits 0, so
# this script refuses any other version. Settings are in about.toml and the
# layout in THIRD-PARTY-NOTICES.hbs.
#
# The generated text is post-processed without changing any licence wording:
#
# - Carriage returns are removed. Some licence files use CRLF, but this
#   repository stores text with LF (.gitattributes), so a CRLF copy would never
#   match after commit.
# - The standard MIT template line "Copyright (c) <year> <copyright holders>"
#   and the blank line after it are removed. cargo-about prints the standard
#   text for crates that publish none, and the line would name nobody; those
#   crates are listed in about.toml. Any other remaining template placeholder
#   stops the script, since that crate needs a clarification in about.toml.
# - The Mochimo licence is appended from LICENSE.md, because cargo-about 0.9.2
#   does not write the text of a LicenseRef licence.
set -euo pipefail

about_version="0.9.2"

mode="write"
case "${1:-}" in
  "") ;;
  --check) mode="check" ;;
  *)
    echo "usage: bash ci/notices.sh [--check]" >&2
    exit 2
    ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

found=$(cargo about --version 2>/dev/null || true)
if [ "$found" != "cargo-about $about_version" ]; then
  echo "ci/notices.sh needs cargo-about $about_version, and found '${found:-no cargo-about}'." >&2
  echo "Install it with: cargo install --locked --features cli cargo-about --version $about_version" >&2
  exit 2
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --workspace: include this repository's crate with its dependencies.
# No --all-features: shipped builds use the default features.
# --fail: stop on a licence about.toml does not accept or cannot be determined;
#   otherwise the crate would be silently omitted with exit status 0.
# --locked: use Cargo.lock exactly.
# -L warn: print warnings. Five are expected: three about the Mochimo licence,
#   and two for objc2 crates whose newer versions have no LICENSE.txt to fetch.
# The `git` clarifications in about.toml fetch files from
# raw.githubusercontent.com, so the run needs the network.
cargo about -L warn generate --workspace --locked --fail \
  -c about.toml -o "$work/generated.txt" THIRD-PARTY-NOTICES.hbs

template_line='Copyright (c) <year> <copyright holders>'
tr -d '\r' < "$work/generated.txt" |
  awk -v line="$template_line" '
    skip && $0 == "" { skip = 0; next }
    { skip = 0 }
    $0 == line { skip = 1; next }
    { print }
  ' > "$work/generated-lf.txt"

if grep -n -i -E '<(year|copyright holders?|owner|name of author)>' "$work/generated-lf.txt" >&2; then
  cat >&2 <<'EOF'
The generated notices still hold a licence template's placeholder (above). A
crate has fallen back to a standard licence text that names no holder. Give it
its own text with a clarification in about.toml.
EOF
  exit 1
fi

cat "$work/generated-lf.txt" LICENSE.md > "$work/THIRD-PARTY-NOTICES.txt"

if [ "$mode" = write ]; then
  cp "$work/THIRD-PARTY-NOTICES.txt" THIRD-PARTY-NOTICES.txt
  echo "wrote THIRD-PARTY-NOTICES.txt"
  exit 0
fi

if ! cmp -s THIRD-PARTY-NOTICES.txt "$work/THIRD-PARTY-NOTICES.txt"; then
  diff -u THIRD-PARTY-NOTICES.txt "$work/THIRD-PARTY-NOTICES.txt" | head -n 60 >&2 || true
  cat >&2 <<'EOF'
THIRD-PARTY-NOTICES.txt is not what Cargo.lock and about.toml give now.
To regenerate it, install cargo-about 0.9.2 as above, then run
  bash ci/notices.sh
and commit the file it writes.
EOF
  exit 1
fi
echo "THIRD-PARTY-NOTICES.txt is current"
