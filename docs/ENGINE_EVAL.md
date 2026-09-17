# Terminal engine evaluation (ADR-0001, M0 gate)

Time-boxed evaluation authorized at the M0 decision review. Harness:
[`tools/engine-eval`](../tools/engine-eval/) (standalone crate; needs one
online build to fetch `alacritty_terminal 0.26.0`). Machine context and
method caveats are the same as [M0_EVIDENCE.md](M0_EVIDENCE.md).

## Candidates

| Candidate | Status |
|---|---|
| `vt100 0.16.2` (incumbent) | Baseline; evaluated in-tree (M0 repros) and here |
| `alacritty_terminal 0.26.0` | Lead candidate; fully evaluated below |
| `wezterm-term` | **Not published to crates.io** — only usable as a git dependency on the full wezterm workspace. Distribution/build cost assessed as high; deferred unless the Alacritty fork scope proves prohibitive |
| `libvterm` | C/FFI; only if both Rust cores fail a hard requirement |
| `vte 0.15` + own screen model | Rejected (de-facto from-scratch emulator, ADR-0001) |

## Results

### 1. Clean-stream agreement (both engines, 7-stream corpus)

6/7 streams agree exactly. The one difference (200-char wrapped line) is an
**API difference, not a rendering divergence**: vt100's `contents()` joins
wrapped physical rows into logical lines, alacritty reports physical rows.
No evidence of semantic disagreement on the corpus.

### 2. Split-at-every-byte restore continuation (vt100 + `state_formatted`)

Checkpoint at every byte boundary, restore into a fresh parser, continue,
compare against an uninterrupted oracle:

| Corpus | Splits | Failed |
|---|---:|---:|
| plain lines | 17 | 0 |
| sgr styles | 46 | 15 |
| wrapped long line | 208 | 0 |
| scrollback fill | 400 | 0 |
| alternate screen roundtrip | 49 | 31 |
| utf8 multibyte | 35 | 10 |
| mixed modes | 48 | 17 |
| **total** | **803** | **73 (9.1%)** |

Failures begin at the first split inside any escape sequence or multi-byte
character and cover the entire alternate-screen residency window. The
incumbent formatted-screen replay is quantitatively not a checkpoint.

### 3. Non-destructive resize (40 history lines, shrink 80→40→80)

- **vt100** with oly's current rebuild-from-formatted strategy: **29 of 40
  history lines lost**.
- **alacritty** with its native `Term::resize` (reflow): **0 lines lost**.

The leading candidate already solves the §5.3/§6.4 history-preservation
defect with a maintained code path.

### 4. Checkpoint feasibility (alacritty 0.26, `serde` feature)

- `Grid<Cell>` **is** serde-serializable: bincode round-trip verified
  (both main and inactive/alternate grids live in `Term`).
- `TermMode` and cells/colors are serde-derived.
- `Term` itself is **not** serializable public API. Full checkpoint needs
  serde support for: `grid`, `inactive_grid`, `active_charset`, `tabs`,
  `mode`, `scroll_region`, `colors`, `cursor_style`, `title(_stack)`,
  `keyboard_mode_stack`, `config`; and skips/adapters for `event_proxy`,
  `selection`, `vi_mode_cursor`, `damage`, `is_focused`. That is ~8–10
  small derive/skip additions — a bounded patch, plausible as an upstream
  feature-gated PR or a narrow pinned fork.
- **Partial-input state lives outside `Term`**: alacritty feeds bytes
  through `vte::ansi::Processor`, whose pending escape/OSC/UTF-8 state is
  not serializable either. Options (M2 decision): (a) extend the fork to
  `vte::Processor` (small, stable crate), or (b) checkpoint only at
  parser-ground boundaries and retain the raw bytes since the last ground
  point in the journal (bounded; partial sequences are short), replaying
  them after restore. Option (b) needs a reliable ground-state signal.

## Conclusions

1. **Alacritty passes every property the incumbent fails** (resize reflow)
   and every property both were tested on (clean-stream agreement), and its
   checkpoint gap is bounded: grid serializes today; the missing piece is a
   small serde patch plus a parser-boundary strategy.
2. **vt100's incumbent restore strategy is quantitatively disqualified**
   (9.1% of byte-boundary splits corrupt the continuation, 100% of
   alt-screen-residency splits) and its resize strategy is destructively
   lossy in practice (72% of history lines in the reflow test).
3. Selection should be ratified once the M2 checkpoint prototype lands:
   either upstream accepts a feature-gated `Term: Serialize` patch or we
   carry a narrow pinned fork. If that proves prohibitive, reopen
   WezTerm (git dependency) before considering libvterm.

## Follow-ups for M2 (ADR-0001 acceptance)

- [ ] Fork/patch `Term: Serialize + Deserialize` (and decide `vte::Processor`
      strategy a vs b) and re-run experiment 2 against alacritty — target
      0/803 failures including alt-screen residency.
- [ ] Unicode width pinning (alacritty uses unicode-width internally;
      confirm a per-session pinned width table is possible).
- [ ] Query handling: alacritty answers some queries via its event proxy
      (`EventListener`); map this onto the central query broker (PLAN §5.4)
      so renderers never respond.
- [ ] Windows/ConPTY build and conformance lane.
