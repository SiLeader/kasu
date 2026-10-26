#!/bin/sh
set -eu
fixture_root=$(mktemp -d /tmp/kasu-oss-metadata.XXXXXX)
cp -R /fixtures/TestGitRepository.git "$fixture_root/source.git"
export KASU_OSS_REPO="$fixture_root/source.git"
exec /out/oss-metadata-test --include-ignored --nocapture "$@"
