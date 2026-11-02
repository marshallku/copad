# alacritty_terminal 0.26.0 + copad patches

The `src/` of the crates.io `alacritty_terminal 0.26.0` release, wired in through `[patch.crates-io]` in the
workspace `Cargo.toml`. The `tests/` directory (46 MB of ref fixtures) was not copied. Every change is
marked "copad patch" in the source.

## Keyboard-stack eviction (decisions #164)

```diff
 // src/term/mod.rs, Handler::push_keyboard_mode
 if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
-    let removed = self.title_stack.remove(0);
+    let removed = self.keyboard_mode_stack.remove(0);
```

0.26.0 evicts from the wrong stack, so the 4097th `CSI > flags u` without a pop panics the pane's I/O thread (an
empty title stack). Fixed upstream, unreleased.

## kitty graphics protocol (decisions #189)

- `term/cell.rs`: `CellExtra.graphics` — the image fragments a cell shows (`GraphicCell`, `GraphicImage`), with
  `Cell::graphics` / `add_graphic` / `remove_graphics`. A cell holding one is not "empty", and dropping the
  other extras keeps the allocation while a fragment is left.
- `term/mod.rs`: `Term::set_apc_handler` (called with every APC string the patched vte parser dispatches, in
  stream order), `Handler::cell_size_pixels` for `CSI 16 t` (answered through `TextAreaSizeRequest`), public
  `mark_fully_damaged`, `scroll_region`, `inactive_grid_mut` and `grids_mut`.
- `term/mod.rs`: DA1 answers `ESC [ ? 62 ; 22 c` (VT220 + ANSI colour, as ghostty and xterm) instead of `?6c`:
  kitten only accepts a DA1 reply longer than three bytes as the end of its graphics probe.

The graphics logic itself lives in `copad-term/src/graphics.rs`. These patches are copad's own, so this
directory stays when upstream releases the keyboard fix.
