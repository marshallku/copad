# alacritty_terminal 0.26.0 + one fix (copad decisions #164)

The `src/` of the crates.io `alacritty_terminal 0.26.0` release, wired in through `[patch.crates-io]` in the
workspace `Cargo.toml`. The only change:

```diff
 // src/term/mod.rs, Handler::push_keyboard_mode
 if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
-    let removed = self.title_stack.remove(0);
+    let removed = self.keyboard_mode_stack.remove(0);
```

0.26.0 evicts from the wrong stack, so the 4097th `CSI > flags u` without a pop panics the pane's I/O thread (an
empty title stack). Upstream master has the fix; delete this directory and the `[patch.crates-io]` entry once a
release that contains it (> 0.26.0) is out. The `tests/` directory (46 MB of ref fixtures) was not copied.
