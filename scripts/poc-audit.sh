#!/usr/bin/env bash
# `just poc-audit` (local only: the PoC files and the audit table are gitignored).
# Every proof-of-concept test function in concepts/research/{poc,round2/poc,round3/poc}/*.rs is
# accounted for: a line under src/ or tests/ carries `PoC:` and the function name as a whole
# word, or its row in specs/026-assurance-and-docs/poc-audit.md has disposition `D` (designed
# out, no regression) and names an H-decision (H1–H22). Prints the missing ones; exit 1 if any.
set -euo pipefail
dirs=(concepts/research/poc concepts/research/round2/poc concepts/research/round3/poc)
table=specs/026-assurance-and-docs/poc-audit.md
present=0
for d in "${dirs[@]}"; do [ -d "$d" ] && present=1; done
if [ "$present" -eq 0 ]; then
    echo "poc-audit skipped: concepts/ not present (local-only check)"
    exit 0
fi
[ -f "$table" ] || { echo "✗ $table missing" >&2; exit 1; }

# file<TAB>fn for every fn that follows #[test] / #[tokio::test] (other attributes may sit between).
list=$(for d in "${dirs[@]}"; do
    for f in "$d"/*.rs; do
        [ -f "$f" ] || continue
        awk -v file="$f" '
            /^[[:space:]]*#\[(tokio::)?test([(\]])/ { armed = 1; next }
            armed && /^[[:space:]]*#\[/ { next }
            armed && /^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]+[A-Za-z0-9_]+/ {
                s = $0; sub(/^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]+/, "", s)
                sub(/[^A-Za-z0-9_].*/, "", s); print file "\t" s; armed = 0; next
            }
            armed && NF { armed = 0 }
        ' "$f"
    done
done)

markers=$(grep -rhE 'PoC:' src tests 2>/dev/null || true)
total=0; missing=0; r=0; dr=0; dd=0; p=0
while IFS=$'\t' read -r file fn; do
    [ -n "$fn" ] || continue
    total=$((total + 1))
    row=$(grep -E "^\|[[:space:]]*[0-9]+[[:space:]]*\|[[:space:]]*\`$fn\`[[:space:]]*\|" "$table" | head -n1 || true)
    disp=$(printf '%s' "$row" | awk -F'|' '{ gsub(/^[[:space:]]+|[[:space:]]+$/, "", $7); print $7 }')
    case "$disp" in
        R) r=$((r + 1)) ;; "D + R") dr=$((dr + 1)) ;; D) dd=$((dd + 1)) ;; P) p=$((p + 1)) ;;
    esac
    if printf '%s\n' "$markers" | grep -qwF -- "$fn"; then
        continue
    fi
    if [ "$disp" = "D" ] && printf '%s' "$row" | grep -qE '(^|[^A-Za-z0-9])H([1-9]|1[0-9]|2[0-2])([^0-9]|$)'; then
        continue
    fi
    echo "✗ $file: $fn — no \`PoC: $fn\` marker under src/ or tests/ and no D row with an H-number" >&2
    missing=$((missing + 1))
done <<< "$list"

unclassified=$((total - r - dr - dd - p))
if [ "$missing" -ne 0 ]; then
    echo "✗ $missing of $total PoC functions unaccounted for" >&2
    exit 1
fi
echo "✓ $total PoC functions accounted for — R $r · D + R $dr · D $dd · P $p (no table row: $unclassified)"
