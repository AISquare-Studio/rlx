#!/usr/bin/env bash
# Runs `cargo test -p rlxd --test here` RUNS times (default 30), each within a time limit
# (RLXD_STRESS_LIMIT seconds, default 300), and fails if a run fails, a run hangs, or an
# rlxd a test started still runs after it.
#
# It builds and tests, so on a shared machine run it through the memory gate:
#
#     rlx fleet gate -- rlxd/tests/stress-here.sh 30
#
# An rlxd a test started is one serving from this workspace's build or from a test's
# temporary home (`$TMPDIR/rlxd-<pid>-<n>`, the copies of rlxd the tests install there).
# Those already running before the first run are left out: another checkout's tests may
# own them.
set -u

runs=${1:-30}
limit=${RLXD_STRESS_LIMIT:-300}
workspace=$(cd "$(dirname "$0")/.." && pwd)
tmp=${TMPDIR:-/tmp}
tmp=${tmp%/}

# `pid command` of each rlxd serving from this workspace or a test's home.
rlxds() {
    ps -A -o pid=,command= |
        awk -v ws="$workspace/" -v tmp="$tmp/rlxd-" \
            '/rlxd serve/ && (index($0, ws) || index($0, tmp)) { print }'
}

# Those of `rlxds` whose pid is not in the list $1.
new_rlxds() {
    rlxds | while read -r pid command; do
        case " $1 " in
            *" $pid "*) ;;
            *) echo "$pid $command" ;;
        esac
    done
}

before=$(rlxds | awk '{ print $1 }' | tr '\n' ' ')
echo "rlxd serving before: ${before:-none}"

cd "$workspace" || exit 1
cargo test -j 3 -p rlxd --test here --no-run || exit 1

failed=0
set -m # each run in a process group of its own, so a hung one is killed whole
for n in $(seq 1 "$runs"); do
    log=$(mktemp "${tmp}/stress-here-$n.XXXXXX")
    started=$(date +%s)
    cargo test -j 3 -p rlxd --test here >"$log" 2>&1 &
    run=$!
    hung=0
    while kill -0 "$run" 2>/dev/null; do
        if (($(date +%s) - started > limit)); then
            hung=1
            echo "run $n: HUNG after ${limit} s; what it was running:"
            ps -A -o pid=,ppid=,pgid=,etime=,command= | awk -v g="$run" '$3 == g'
            kill -9 -- "-$run" 2>/dev/null
            break
        fi
        sleep 1
    done
    wait "$run"
    code=$?
    took=$(($(date +%s) - started))
    # A stop returns once rlxd lets its lock go, which it does last: give it a moment to
    # exit.
    sleep 2
    left=$(new_rlxds "$before")
    if ((hung)) || ((code != 0)) || [ -n "$left" ]; then
        failed=$((failed + 1))
        echo "run $n: FAILED (exit $code, ${took} s)"
        grep -E "^test .*(FAILED|ok)$|panicked|test result" "$log" | grep -v " ok$" | tail -20
        if [ -n "$left" ]; then
            echo "run $n: rlxd left running:"
            echo "$left"
        fi
        echo "run $n: log kept at $log"
    else
        echo "run $n: ok (${took} s) $(grep -E '^test result' "$log" | tail -1)"
        rm -f "$log"
    fi
done

after=$(new_rlxds "$before")
echo "rlxd serving before: ${before:-none}"
echo "rlxd left after $runs runs: ${after:-none}"
if ((failed)) || [ -n "$after" ]; then
    echo "FAIL: $failed of $runs runs failed"
    exit 1
fi
echo "PASS: $runs of $runs runs, no rlxd left"
