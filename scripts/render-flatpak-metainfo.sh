#!/usr/bin/env bash
#
# Render packaging/flatpak/io.github.ja7ad.hydra.metainfo.xml with a concrete
# version and release date baked in.
#
# Usage:
#   scripts/render-flatpak-metainfo.sh                            # version from Cargo.toml, date today (UTC)
#   scripts/render-flatpak-metainfo.sh 0.5.0                      # explicit version
#   scripts/render-flatpak-metainfo.sh 0.5.0 out.xml              # explicit version and output file
#   scripts/render-flatpak-metainfo.sh 0.5.0 out.xml 2026-09-14   # explicit version, output file, and date
#
# Writes to stdout when no output path is given.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
xml_in="$repo_root/packaging/flatpak/io.github.ja7ad.hydra.metainfo.xml"
v_placeholder='@HYDRA_VERSION@'
d_placeholder='@HYDRA_DATE@'

version=${1:-}
out=${2:-}
date=${3:-}

if [ -z "$version" ]; then
  version=$(grep -m1 '^version = ' "$repo_root/Cargo.toml" | cut -d'"' -f2)
fi
version="${version#v}"

if [ -z "$date" ]; then
  date=$(date -u +%Y-%m-%d)
fi

if [ -z "$version" ]; then
  echo "render-flatpak-metainfo: could not determine a version" >&2
  exit 1
fi

if ! grep -q "$v_placeholder" "$xml_in"; then
  echo "render-flatpak-metainfo: $v_placeholder not found in $xml_in" >&2
  exit 1
fi

rendered=$(sed -e "s/$v_placeholder/$version/g" -e "s/$d_placeholder/$date/g" "$xml_in")

if printf '%s\n' "$rendered" | grep -q -E "($v_placeholder|$d_placeholder)"; then
  echo "render-flatpak-metainfo: placeholder survived substitution" >&2
  exit 1
fi

if [ -n "$out" ]; then
  printf '%s\n' "$rendered" > "$out"
  echo "render-flatpak-metainfo: wrote $out (version $version, date $date)" >&2
else
  printf '%s\n' "$rendered"
fi
