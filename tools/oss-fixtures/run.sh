#!/bin/sh
set -eu
# Bind-mount ownership differs across Docker hosts. Copy the task-owned bare
# fixture into this disposable container rather than bypassing Git ownership checks.
fixture_work=$(mktemp -d /tmp/kasu-oss.XXXXXX)
cp -R /fixtures/submodules.git "$fixture_work/submodules.git"
cp -R /fixtures/byte-paths.git "$fixture_work/byte-paths.git"
export KASU_OSS_BYTE_PATH_REPO="$fixture_work/byte-paths.git"
export KASU_OSS_SUBMODULE_REPO="$fixture_work/submodules.git"
exec /out/roundtrip-test --include-ignored --skip pinned_oss_repository_roundtrips_all_objects_through_pack_and_clone "$@"
