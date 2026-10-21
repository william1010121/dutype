#!/usr/bin/env bash
# Builds an isolated source copy. No terminal backend or external file actions.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/dutype-filename-display.XXXXXX")
printf 'Isolated benchmark build: %s\n' "$work" >&2
cp "$repo/Cargo.toml" "$repo/Cargo.lock" "$work/"
cp -R "$repo/src" "$work/src"
cp "$repo/bench/filename-display/harness.rs" "$work/src/filename_display.rs"
git -C "$repo" show e0bd9747ba3ba2f740c18940f65dace0ee528e1d:src/ui.rs > "$work/src/ui_baseline.rs"
git -C "$repo" show e0bd9747ba3ba2f740c18940f65dace0ee528e1d:src/report.rs > "$work/src/report_baseline.rs"
python3 - "$work" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])
s = (root/'src/ui.rs').read_text()
needle = "fn entry_name(c: &Node) -> Line<'_> {"
assert s.count(needle) == 1
s = s.replace(needle, needle + '\n crate::ENTRY_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);')
(root/'src/ui_probe.rs').write_text(s)
PY
cat >> "$work/Cargo.toml" <<'TOML'

[[bin]]
name = "filename_display"
path = "src/filename_display.rs"
TOML
: "${CARGO_TARGET_DIR:=$work/target}"
export CARGO_TARGET_DIR
cargo +1.99.1 build --offline --locked --manifest-path "$work/Cargo.toml" --release --bin filename_display
"$CARGO_TARGET_DIR/release/filename_display" "${1:-verify}"
