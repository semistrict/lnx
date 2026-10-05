#!/bin/zsh
# Publishes an lnx release from this Mac: bumps the version, builds the
# signed and notarized binary (build-distribution.sh), points the Homebrew
# formula at it, commits, tags, pushes, and creates the GitHub release.
#
#   scripts/release.sh 0.3.1
#
# Run it from a clean main that matches origin/main.

set -euo pipefail

die() {
    echo "release: $*" >&2
    exit 1
}

version=${1:-}
[[ $version =~ '^[0-9]+\.[0-9]+\.[0-9]+$' ]] || die "usage: scripts/release.sh X.Y.Z"
tag="v$version"

script_directory=${0:A:h}
cd "${script_directory:h}"

[[ $(git branch --show-current) == main ]] || die "run from main"
git diff --quiet && git diff --cached --quiet || die "commit or discard local changes first"
git fetch --quiet origin main
[[ $(git rev-parse HEAD) == $(git rev-parse origin/main) ]] || die "main does not match origin/main"
git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"

release_files=(Cargo.toml Cargo.lock lnx-protocol/Cargo.toml guest-agent/Cargo.toml Formula/lnx.rb)
# Until the release commit exists, a failure puts the files back.
trap 'git checkout --quiet -- "${release_files[@]}"' EXIT
for manifest in Cargo.toml lnx-protocol/Cargo.toml guest-agent/Cargo.toml; do
    perl -0pi -e "s/^version = \"[^\"]*\"/version = \"$version\"/m" "$manifest"
done

tarball=$("$script_directory/build-distribution.sh")
sha=$(cut -d' ' -f1 "$tarball.sha256")
perl -pi -e "s/^  version \".*\"/  version \"$version\"/; s/^  sha256 \".*\"/  sha256 \"$sha\"/" Formula/lnx.rb
ruby -c Formula/lnx.rb >/dev/null

git add "${release_files[@]}"
git commit --quiet -m "Release $tag"
trap - EXIT
git tag -a "$tag" -m "lnx $tag"
git push --quiet origin main "$tag"
gh release create "$tag" "$tarball" "$tarball.sha256" \
    --verify-tag --latest --title "lnx $tag" --generate-notes

echo "released $tag"
