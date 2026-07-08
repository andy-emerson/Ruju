#!/usr/bin/env sh
# Fetch and verify the pinned build-time Julia (design/strategy.md, D1
# distribution note): one immutable release asset, checksum-verified, built
# once by .github/workflows/build-pinned-julia.yml. Unpacks into
# tools/pinned-julia/ (gitignored); prints the julia binary path on success.
#
# The expected checksum lives in tools/pinned-julia.sha256, committed after
# the first successful build. If it still holds the placeholder, the artifact
# has not been published yet — run the workflow first.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
tag="pinned-julia-d99fded"
artifact="julia-d99fded-linux-x86_64.tar.gz"
url="https://github.com/andy-emerson/ruju/releases/download/$tag/$artifact"
dest="$here/pinned-julia"

expected="$(cut -d' ' -f1 "$here/pinned-julia.sha256")"
if [ "$expected" = "PLACEHOLDER" ]; then
    echo "tools/pinned-julia.sha256 is unset: publish the artifact first" >&2
    echo "(.github/workflows/build-pinned-julia.yml), then commit its checksum." >&2
    exit 1
fi

if [ -x "$dest/bin/julia" ]; then
    echo "$dest/bin/julia"
    exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$artifact" "$url"
actual="$(sha256sum "$tmp/$artifact" | cut -d' ' -f1)"
if [ "$actual" != "$expected" ]; then
    echo "checksum mismatch for $artifact: expected $expected, got $actual" >&2
    exit 1
fi

mkdir -p "$dest"
tar -xzf "$tmp/$artifact" -C "$dest" --strip-components 1
"$dest/bin/julia" --version >&2
echo "$dest/bin/julia"
