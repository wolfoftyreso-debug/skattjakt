# Which build of a binary a suite should run.
#
# Why this exists
# ===============
#
# Suites picked a binary by profile: some preferred `target/release`, some
# hardcoded `target/debug`. Either way the choice ignored *when* the binary was
# built, so a suite would happily start a binary from a week ago, pass its
# health check, and then fail in ways that read as product bugs.
#
# That has now cost four debugging sessions in this repository. Once it looked
# like broken shop pages; once like a broken database trigger; once like an
# empty report section that had in fact just been fixed. Every time the product
# was fine and the test was running yesterday's code.
#
# So: whichever build is newer, never whichever profile is preferred. A suite
# that wants a specific profile can still name the path itself.
#
# Usage:
#   source "$ROOT/tests/lib/newest-binary.sh"
#   API="$(newest_binary skattjakt-api)"

newest_binary() { # name
    local name="$1"
    local root="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
    local release="$root/target/release/$name"
    local debug="$root/target/debug/$name"

    local chosen=""
    [[ -x "$release" ]] && chosen="$release"
    [[ -x "$debug" && ( -z "$chosen" || "$debug" -nt "$chosen" ) ]] && chosen="$debug"

    if [[ -z "$chosen" ]]; then
        echo "neither target/release/$name nor target/debug/$name is built" >&2
        echo "build it first: cargo build --bin $name" >&2
        return 1
    fi
    printf '%s\n' "$chosen"
}
