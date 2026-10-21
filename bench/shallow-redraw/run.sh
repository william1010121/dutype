#!/usr/bin/env bash
# Standalone build copy. Never changes production Cargo.toml or builds in it.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/dutype-shallow-redraw.XXXXXX")
printf 'Isolated benchmark build: %s\n' "$work" >&2
cp "$repo/Cargo.toml" "$repo/Cargo.lock" "$work/"
cp -R "$repo/src" "$work/src"
mkdir -p "$work/bench/shallow-redraw" "$work/empty-home" "$work/empty-config" "$work/empty-cache" "$work/empty-path"
cp "$repo/bench/shallow-redraw/harness.rs" "$work/src/shallow_redraw.rs"
git -C "$repo" show d4cd241fa981c3d7074c1a6c396ca345af5861f7:src/ui.rs > "$work/bench/shallow-redraw/ui_baseline.rs"
python3 - "$work" <<'PY'
from pathlib import Path
import sys
root=Path(sys.argv[1]);s=(root/'src/ui.rs').read_text()
for signature,counter in [("fn entry_name(c: &Node) -> Line<'static> {",'ENTRY_CALLS'),('fn apportion(cats: &[u64], len: usize) -> Vec<(Category, usize)> {','APPORTION_CALLS')]:
    assert s.count(signature)==1, f'Expected one instrumentation point: {signature}'
    s=s.replace(signature,signature+f'\n    crate::{counter}.fetch_add(1, std::sync::atomic::Ordering::Relaxed);')
(root/'bench/shallow-redraw/ui_probe.rs').write_text(s)
PY
cat >> "$work/Cargo.toml" <<'TOML'

[[bin]]
name = "shallow_redraw"
path = "src/shallow_redraw.rs"
TOML
# Linux-only research harness: process CPU clock is not a production dependency.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$work/target}"
cargo +1.99.1 build --offline --locked --manifest-path "$work/Cargo.toml" --release --bin shallow_redraw
HOME="$work/empty-home" XDG_CONFIG_HOME="$work/empty-config" XDG_CACHE_HOME="$work/empty-cache" PATH="$work/empty-path" \
    "$CARGO_TARGET_DIR/release/shallow_redraw" "${1:-verify}"
