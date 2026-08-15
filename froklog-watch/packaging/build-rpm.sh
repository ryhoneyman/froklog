#!/usr/bin/env bash
# Build the Fedora package inside a Fedora container, from the repo root:
#   ./froklog-watch/packaging/build-rpm.sh
# The RPM lands in ./froklog-watch/packaging/out/.
set -euo pipefail
cd "$(dirname "$0")/../.."
OUT="froklog-watch/packaging/out"
mkdir -p "$OUT"
GITREV=$(git rev-parse --short HEAD 2>/dev/null || echo local)
git archive --format=tar -o "$OUT/froklog-src.tar" HEAD
docker run --rm -v "$PWD/$OUT":/out -v "$PWD/froklog-watch/packaging/froklog-watch.spec":/spec.spec:ro \
    fedora:42 bash -ec '
      dnf install -y -q cargo rust gcc openssl-devel pkgconf-pkg-config dbus-devel libxkbcommon-devel rpm-build
      mkdir -p /root/rpmbuild/SOURCES
      cp /out/froklog-src.tar /root/rpmbuild/SOURCES/
      rpmbuild -bb --define "gitrev '"$GITREV"'" /spec.spec
      cp /root/rpmbuild/RPMS/*/froklog-watch-*.rpm /out/
    '
ls -la "$OUT"/*.rpm
