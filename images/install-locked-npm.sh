#!/bin/sh
set -eu

package=${1:?usage: capscode-install-locked-npm <package> <version> <integrity> <resolved-before> <enabled|ignore>}
version=${2:?missing package version}
integrity=${3:?missing package integrity}
before=${4:?missing dependency cutoff}
scripts=${5:?missing lifecycle script policy}

case "$scripts" in
  enabled|ignore) ;;
  *) echo "capsule.all: lifecycle policy must be enabled or ignore" >&2; exit 2 ;;
esac
[ -n "$integrity" ] || { echo "capsule.all: missing integrity for $package" >&2; exit 1; }
[ -n "$before" ] || { echo "capsule.all: missing resolved_before for $package" >&2; exit 1; }

packed=$(npm pack --silent --ignore-scripts --pack-destination /tmp "$package@$version")
tarball="/tmp/$packed"
actual="sha512-$(node -e 'const fs=require("fs"), crypto=require("crypto"); process.stdout.write(crypto.createHash("sha512").update(fs.readFileSync(process.argv[1])).digest("base64"))' "$tarball")"
if [ "$actual" != "$integrity" ]; then
  echo "capsule.all: integrity mismatch for $package@$version" >&2
  rm -f "$tarball"
  exit 1
fi

if [ "$scripts" = ignore ]; then
  npm install -g --no-fund --no-audit --ignore-scripts --before="$before" "$tarball"
else
  npm install -g --no-fund --no-audit --before="$before" "$tarball"
fi
rm -f "$tarball"
npm cache clean --force
