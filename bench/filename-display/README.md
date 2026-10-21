# Filename display correctness and cost

`run.sh verify` and `run.sh measure` build an isolated source copy with locked,
offline dependencies and explicit Rust 1.99.1. Configure CARGO_HOME/RUSTUP_HOME
and put that cargo on PATH; CARGO_TARGET_DIR may select a disposable target.
The baseline commit `e0bd9747ba3ba2f740c18940f65dace0ee528e1d` must be available
locally. The script leaves the build copy for inspection, never modifies the
production manifest, and does not change global toolchain defaults.

Every render uses ratatui TestBackend, never a terminal backend or external
opener/clipboard. Affected baseline strings exist only in memory; emitted
results are JSON numbers/booleans, not rendered control sequences.

## Verification

- 176 exact full styled-buffer comparisons against the baseline, including
  scanning, ordinary multilingual/combining/emoji filenames, navigation,
  extension/category switches, popup opening/jumping, 0/1/40/10,000 children,
  and 240×40, 120×40, 80×24, and 10×5 views. Table states also match.
- Full/category ordinary text reports are byte-identical; ordinary report JSON
  serialization is byte-identical.
- A 10,000-row affected-name frame formats 33 entry names (visible rows plus
  selected details). The offscreen formatting optimization is retained.
- Durable tests cover exact escape policy, literal-backslash collisions,
  ordinary Unicode borrowing, root/child/nested popup paths, controls and bidi
  in Table and Paragraph cells, extension labels/titles, status escaping once,
  raw clipboard payloads through intercepted key handlers, raw jump identities,
  JSON/MCP/cache paths, leading-dash CLI arguments, and existing lossy aliases.
- Windows path-policy cases run on every platform; native Windows path cases
  are cfg(windows) tests for Windows CI. They were not run natively in the Linux
  measurement environment. Windows separators remain structural; rendered full
  paths are not claimed to be reversible or universally distinct.

## Bounded Linux measurement

The recorded release run used seven alternating baseline/candidate batches.
Names use 50,000 iterations/batch; frames use 20 iterations/batch after five
warmups. Allocation counters run separately from timings. `summary.json`
retains medians and allocation counts; raw samples remain in the local evidence.
These are local microbenchmarks, not platform-wide guarantees.

| Case | Before | After | Allocations before → after |
| --- | ---: | ---: | ---: |
| Ordinary ASCII name | 10.3 ns | 31.2 ns | 1 → 0 |
| Ordinary Unicode name | 9.2 ns | 37.1 ns | 1 → 0 |
| Affected name | 9.6 ns | 69.9 ns | 1 → 1 (+1 realloc) |
| 320-byte control/backslash-heavy name | 12.9 ns | 1,781.5 ns | 1 → 1 (+2 reallocs) |
| Ordinary 10,000-row 120×40 frame | 347.7 µs | 352.6 µs | 974 → 903 |
| Affected 10,000-row 120×40 frame | 329.7 µs | 350.5 µs | 928 → 926 |

Checking ordinary text costs CPU even when it stays borrowed; the visible-frame
change was +1.4% in this run, with fewer allocations. Affected text expands and
costs more to format (+6.3% for the affected frame); the deliberately dense
control-name case is much slower than merely cloning unsafe raw text. No scan,
classification, filesystem, cache, or machine-output hot path calls the helper.
