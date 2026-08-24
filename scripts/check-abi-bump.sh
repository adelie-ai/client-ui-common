#!/usr/bin/env bash
# Catch the common ABI-bump omission: an extern "C" declaration added,
# removed, or re-signed in ffi/src/ without bumping ADELE_CORE_ABI_VERSION.
# See "C ABI version bump policy" in AGENTS.md for the policy this checks.
#
# Stated limits. Do not read a passing run as more coverage than this:
#
#   1. It runs only when someone runs `just check`. `just install-hooks` does
#      not exist yet (tracked in #88), so nothing forces a contributor to run
#      this before pushing.
#   2. It compares the generated header against `git show HEAD:...`, the last
#      commit. An un-bumped change that is already committed passes on every
#      later run, because the check then compares the un-bumped header
#      against itself. This repository has no CI, so nothing else closes
#      that hole.
#   3. A semantics change with no signature change produces no header diff at
#      all, so this script cannot see it. Review is the only thing that
#      catches that case.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

header_path="ffi/include/adele_client_core.h"

# Regenerate the header via the normal build so it reflects the working tree,
# not whatever was last committed.
cargo build -p client-ui-ffi --quiet

if ! git cat-file -e "HEAD:${header_path}" 2>/dev/null; then
    echo "check-abi-bump: no committed header at HEAD:${header_path} yet; nothing to compare" >&2
    exit 0
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

git show "HEAD:${header_path}" >"$work_dir/head.h"
cp "$header_path" "$work_dir/working.h"

# "Declaration lines": every non-comment, non-blank line that is not
# preprocessor plumbing (#include/#ifdef/#define-without-value). In this
# generated header every declaration (typedef, function prototype) sits on
# one physical line, ends with ';', and is not indented; doc-comment body
# lines are indented (or start with '/*'/'*/'), so this filter isolates
# declarations from documentation without needing a C parser.
declarations() {
    grep -E '^[^ ].*;$' "$1" || true
}

# The integer value of `#define ADELE_CORE_ABI_VERSION <N>`. Empty if the
# header carries no such line.
version_value() {
    sed -n 's/^#define ADELE_CORE_ABI_VERSION \([0-9][0-9]*\)$/\1/p' "$1"
}

old_declarations="$(declarations "$work_dir/head.h")"
new_declarations="$(declarations "$work_dir/working.h")"

if [ "$old_declarations" = "$new_declarations" ]; then
    echo "check-abi-bump: no declaration change in ${header_path}"
    exit 0
fi

old_version="$(version_value "$work_dir/head.h")"
new_version="$(version_value "$work_dir/working.h")"

fail() {
    echo "check-abi-bump: $1" >&2
    echo "See 'C ABI version bump policy' in AGENTS.md." >&2
    echo >&2
    echo "--- declarations at HEAD ---" >&2
    echo "$old_declarations" >&2
    echo "--- declarations in the working tree ---" >&2
    echo "$new_declarations" >&2
    exit 1
}

if [ -z "$new_version" ]; then
    fail "${header_path} declarations changed and no longer carries a #define ADELE_CORE_ABI_VERSION line."
fi

if [ -z "$old_version" ]; then
    # The constant did not exist at HEAD, so there is no prior value to
    # compare against - this is the constant's own introduction.
    echo "check-abi-bump: declarations changed; ADELE_CORE_ABI_VERSION is newly present (${new_version})"
    exit 0
fi

if [ "$new_version" -ne $((old_version + 1)) ]; then
    fail "declarations changed but ADELE_CORE_ABI_VERSION went from ${old_version} to ${new_version}, not up by exactly one."
fi

echo "check-abi-bump: declarations changed and ADELE_CORE_ABI_VERSION went from ${old_version} to ${new_version}"
