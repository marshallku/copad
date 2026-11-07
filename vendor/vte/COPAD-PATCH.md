# vte 0.15.0 + APC dispatch and CSI 16 t (copad, kitty graphics protocol)

The `src/` of the crates.io `vte 0.15.0` release, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml` (the `[[example]]` entry was dropped, `examples/` and `tests/` were not copied). Changes,
all marked "copad patch" in the source:

- `lib.rs`: with the `std` feature, `ESC _` starts an APC string that is collected (up to `MAX_APC_RAW`)
  and handed to the new `Perform::apc_dispatch` when it ends with `ESC \`. CAN/SUB, or an ESC followed by
  anything but a backslash, cancels it; an oversized one is consumed to its terminator and dropped. SOS and
  PM are still ignored.
- `lib.rs` / `ansi.rs`: `Parser::is_ground` / `Processor::is_ground` — between sequences and UTF-8
  characters, where copad-linux's relay may splice its own sequences into the stream (decisions #190).
- `lib.rs`: `advance_partial_utf8` returns `c.len_utf8() - old_bytes` where upstream returns
  `valid_bytes - old_bytes`. When a read ended inside a character and the next one held that character's
  rest, a whole character and the start of another, upstream consumed the whole one without printing it
  (`é` then a space then CJK, split inside `é`, lost the space).
- `ansi.rs`: `Handler::apc_dispatch` (forwarded from the Performer) and `Handler::cell_size_pixels` for
  `CSI 16 t`; both default to no-ops.

Why at the parser and not a pre-scan of the PTY bytes: a synchronized update (`?2026`) buffers raw bytes
and replays them through the parser, so only a parser-level hook keeps image commands in stream order with
the cursor moves around them.

Tests: copy this directory out of the workspace and run `cargo test --offline --features ansi`.
