#!/usr/bin/env bash
# Enforces house style rules that rustfmt/clippy can't express.
#
# Rule: no one-line conditional expressions, e.g. `let x = if a { b } else { c };`.
# Use a multi-line if/else block or a match instead.
set -euo pipefail
cd "$(dirname "$0")/.."

pattern='\bif\b[^{]*\{[^}]*\}\s*else\s*\{[^}]*\}'
if matches=$(grep -rnP --include='*.rs' "$pattern" crates); then
    echo "error: one-line if/else expressions are not allowed (use a block or match):"
    echo "$matches"
    exit 1
fi
echo "style ok"
