#!/usr/bin/env bash
# Publish the seven npm packages for one supdoc release from that release's GitHub assets.
#
#   npm/publish.sh <version> [--dry-run] [--provenance]
#
# <version> is the release version without a leading v, for example 0.6.0. --dry-run runs
# `npm publish --dry-run` and publishes nothing. --provenance asks npm for a provenance
# statement, which needs the OIDC token of a GitHub Actions job; a local run omits it.
set -euo pipefail

usage() {
  echo "usage: npm/publish.sh <version> [--dry-run] [--provenance]   (version like 0.6.0, no leading v)" >&2
  exit 2
}

version="" dry_run=false provenance=false
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=true ;;
    --provenance) provenance=true ;;
    -*) usage ;;
    *) [ -z "$version" ] || usage; version="$arg" ;;
  esac
done
[ -n "$version" ] || usage
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "version '$version' is not semver like 0.6.0 (no leading v)" >&2; usage; }

tag="v$version"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

work="$(mktemp -d "${TMPDIR:-/tmp}/supdoc-npm-publish.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir "$work/assets" "$work/out"

gh release download "$tag" --repo aidenzepp/supdoc --dir "$work/assets"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$work/assets" && sha256sum -c SHA256SUMS)
else
  (cd "$work/assets" && shasum -a 256 -c SHA256SUMS)
fi

# SHA256SUMS comes from the same release as the archives, so it can't catch a swapped asset. The
# release workflow attests each archive it builds; refuse any archive without that attestation.
for archive in "$work/assets"/supdoc-*.tar.gz; do
  gh attestation verify "$archive" --repo aidenzepp/supdoc >/dev/null
  echo "attested: $(basename "$archive")"
done

tarballs=()
while IFS= read -r line; do tarballs+=("$line"); done < <(
  node npm/build.mjs --tag "$tag" --assets "$work/assets" --license LICENSE --out "$work/out"
)
if [ "${#tarballs[@]}" -ne 7 ]; then
  echo "expected 7 tarballs from build.mjs, got ${#tarballs[@]}" >&2
  exit 1
fi

flags=(--access public)
if [ "$provenance" = true ]; then flags+=(--provenance); else flags+=(--provenance=false); fi
if [ "$dry_run" = true ]; then flags+=(--dry-run); fi

for tarball in "${tarballs[@]}"; do
  name="$(tar -xzOf "$work/out/$tarball" package/package.json | node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync(0, "utf8")).name)')"
  # A re-run after a partial publish skips what is already on the registry.
  if npm view "$name@$version" version >/dev/null 2>&1; then
    echo "== $name@$version is already published; skipping"
    continue
  fi
  echo "== $name@$version"
  npm publish "${flags[@]}" "$work/out/$tarball"
done

echo "packages for $tag:"
for tarball in "${tarballs[@]}"; do echo "  $tarball"; done
if [ "$dry_run" = true ]; then echo "dry run: nothing published"; fi
