# engine-eval

Isolated ADR-0001 prototype: compares the incumbent `vt100` engine against
the lead candidate `alacritty_terminal` on the properties the oly 1.0 plan
requires (clean-stream agreement, split-at-every-byte restore continuation,
non-destructive resize, checkpoint feasibility).

This is a **standalone crate, intentionally not part of the main package**:
it requires network access to fetch `alacritty_terminal` (the main crate
builds offline). Results are recorded in
[`docs/ENGINE_EVAL.md`](../../docs/ENGINE_EVAL.md); re-run after upgrading
candidates or extending the corpus:

```sh
cd tools/engine-eval
cargo run --release
```

See `src/main.rs` for the corpus and experiment definitions. The wrapped-line
"DIFFER" in experiment 1 is an API difference, not a semantic divergence:
vt100 reports wrapped physical rows as one logical line while alacritty
reports physical rows.
