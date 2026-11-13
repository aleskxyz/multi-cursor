#!/usr/bin/env bash
# Show or set Multi Cursor's version. It is stored in five files that must
# agree: package.json, package-lock.json, src-tauri/tauri.conf.json,
# src-tauri/Cargo.toml and src-tauri/Cargo.lock.
#
#   scripts/version.sh          # print the version; fails if the files disagree
#   scripts/version.sh 1.2.0    # set it in all five (1.2.0-beta.1 for a pre-release)
#
# Only edits files: no commit, no tag. The Prepare release workflow runs it
# for you (see CONTRIBUTING.md); running it locally works the same way.
set -euo pipefail
cd "$(dirname "$0")/.."

# One "<file> <version>" line per file.
versions() {
  echo "package.json $(node -p "require('./package.json').version")"
  echo "package-lock.json $(node -p "require('./package-lock.json').version")"
  echo "src-tauri/tauri.conf.json $(node -p "require('./src-tauri/tauri.conf.json').version")"
  echo "src-tauri/Cargo.toml $(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' src-tauri/Cargo.toml)"
  echo "src-tauri/Cargo.lock $(awk '/^name = "multi-cursor"$/ { getline; gsub(/version = |"/, ""); print; exit }' src-tauri/Cargo.lock)"
}

# The version all five files agree on, or an error listing them.
current_version() {
  local list
  list="$(versions)"
  if [ "$(awk '{print $2}' <<<"$list" | sort -u | wc -l)" -ne 1 ]; then
    echo "error: the version files disagree:" >&2
    sed 's/^/  /' <<<"$list" >&2
    exit 1
  fi
  awk 'NR == 1 {print $2}' <<<"$list"
}

CURRENT="$(current_version)"
if [ $# -eq 0 ]; then
  echo "$CURRENT"
  exit 0
fi

VERSION="${1#v}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: '$1' is not a version like 1.2.0 or 1.2.0-beta.1" >&2
  exit 1
fi
# SemVer order, as the app's update check sees it: a release is newer than
# its own pre-releases (1.2.0 > 1.2.0-beta.1).
if ! node -e '
  const parse = (v) => {
    const [core, pre] = v.split(/-(.*)/s);
    return { nums: core.split(".").map(Number), pre: pre ?? null };
  };
  const [a, b] = process.argv.slice(1).map(parse);
  for (let i = 0; i < 3; i++) if (a.nums[i] !== b.nums[i]) process.exit(a.nums[i] > b.nums[i] ? 0 : 1);
  if (a.pre === null || b.pre === null) process.exit(a.pre === null && b.pre !== null ? 0 : 1);
  const order = a.pre.localeCompare(b.pre, "en", { numeric: true });
  process.exit(order > 0 ? 0 : 1);
' "$VERSION" "$CURRENT"; then
  echo "error: $VERSION is not newer than the current version $CURRENT" >&2
  exit 1
fi

# package.json + package-lock.json
npm version "$VERSION" --no-git-tag-version --allow-same-version >/dev/null

# tauri.conf.json (2-space JSON, key order kept)
node -e '
  const fs = require("fs");
  const p = "src-tauri/tauri.conf.json";
  const conf = JSON.parse(fs.readFileSync(p, "utf8"));
  conf.version = process.argv[1];
  fs.writeFileSync(p, JSON.stringify(conf, null, 2) + "\n");
' "$VERSION"

# Cargo.toml: only the version line of [package]
awk -v v="$VERSION" '
  /^\[/ { section = $0 }
  section == "[package]" && !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
  { print }
' src-tauri/Cargo.toml > src-tauri/Cargo.toml.tmp && mv src-tauri/Cargo.toml.tmp src-tauri/Cargo.toml

# Cargo.lock: only this crate's entry (no network, no other dependency changes)
awk -v v="$VERSION" '
  prev == "name = \"multi-cursor\"" && /^version = / { print "version = \"" v "\""; prev = ""; next }
  { print; prev = $0 }
' src-tauri/Cargo.lock > src-tauri/Cargo.lock.tmp && mv src-tauri/Cargo.lock.tmp src-tauri/Cargo.lock

NEW="$(current_version)"
if [ "$NEW" != "$VERSION" ]; then
  echo "error: expected $VERSION after the update, found $NEW" >&2
  exit 1
fi
echo "$CURRENT -> $VERSION"
