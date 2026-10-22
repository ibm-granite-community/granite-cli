#!/usr/bin/env bash
# Merge LCOV tracefiles and summarise their line coverage. The CI coverage
# job uses it to combine the unit and end-to-end test coverage.
#
# Usage:
#   scripts/lcov.sh merge OUT IN...   write the union of the INs' line data
#                                     to OUT
#   scripts/lcov.sh percent FILE      print FILE's line coverage, in percent
#   scripts/lcov.sh lines FILE        print FILE's covered and instrumented
#                                     line counts
#
# merge keeps only the line records (SF, DA, LF, LH): a line is in OUT when
# any input has it, and its hit count is the sum of the inputs' counts.
# Function and branch records are dropped, because the inputs come from
# different builds whose function records do not line up.
#
# percent and lines count DA records, so they give the same result for a
# merged file and for one written by `cargo llvm-cov report --lcov`.

set -euo pipefail

# Prints "file<TAB>line<TAB>count" for every DA record in the given files.
da_records() {
    awk -F, '
        /^SF:/ { sf = substr($0, 4); next }
        /^DA:/ { printf "%s\t%s\t%s\n", sf, substr($1, 4), $2 }
    ' "$@"
}

cmd=${1:-}
case "$cmd" in
    merge)
        [ $# -ge 3 ] || { echo "usage: $0 merge OUT IN..." >&2; exit 2; }
        out=$2
        shift 2
        da_records "$@" | LC_ALL=C sort -t "$(printf '\t')" -k1,1 -k2,2n | awk -F '\t' '
            function line_done() {
                if (key == "") return
                printf "DA:%s,%.0f\n", line, count
                lf++
                if (count > 0) lh++
            }
            function file_done() {
                line_done()
                if (sf != "") printf "LF:%d\nLH:%d\nend_of_record\n", lf, lh
            }
            $1 SUBSEP $2 != key {
                if ($1 != sf) {
                    file_done()
                    sf = $1
                    lf = 0
                    lh = 0
                    printf "SF:%s\n", sf
                } else {
                    line_done()
                }
                key = $1 SUBSEP $2
                line = $2
                count = 0
            }
            { count += $3 }
            END { file_done() }
        ' > "$out"
        ;;
    percent | lines)
        [ $# -eq 2 ] || { echo "usage: $0 $cmd FILE" >&2; exit 2; }
        da_records "$2" | awk -F '\t' -v cmd="$cmd" '
            { total++; if ($3 > 0) covered++ }
            END {
                if (cmd == "lines") printf "%d %d\n", covered, total
                else printf "%.4f\n", total ? 100 * covered / total : 0
            }
        '
        ;;
    *)
        sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//' >&2
        exit 2
        ;;
esac
