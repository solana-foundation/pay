#!/usr/bin/env bash
set -euo pipefail

coverage_file="${1:-coverage.out}"
threshold="${2:-70}"

if [[ ! -f "${coverage_file}" ]]; then
  echo "coverage file not found: ${coverage_file}" >&2
  exit 1
fi

total_line="$(go tool cover -func="${coverage_file}" | tail -n 1)"
total_percent="$(awk '{print $3}' <<<"${total_line}" | tr -d '%')"

python3 - <<'PY' "${total_percent}" "${threshold}"
import sys

total = float(sys.argv[1])
threshold = float(sys.argv[2])

if total < threshold:
    print(f"coverage threshold failed: {total:.1f}% < {threshold:.1f}%")
    sys.exit(1)

print(f"coverage threshold passed: {total:.1f}% >= {threshold:.1f}%")
PY
