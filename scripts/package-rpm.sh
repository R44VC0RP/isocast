#!/bin/sh
# On Fedora: dnf builddep packaging/rpm/isocast.spec, then run this from
# the repository root. Package the committed tree (not uncommitted edits).
set -eu
version=$(git show HEAD:Cargo.toml | sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p')
test -n "$version"
mkdir -p dist
out=$(cd dist && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/SOURCES"
git show HEAD:packaging/rpm/isocast.spec | sed "s/^Version:.*/Version:        $version/" > "$work/isocast.spec"
git archive --format=tar.gz --prefix="isocast-$version/" -o "$work/SOURCES/isocast-$version.tar.gz" HEAD
rpmbuild -ba --define "_topdir $work" --define "_srcrpmdir $out" --define "_rpmdir $out" "$work/isocast.spec"
