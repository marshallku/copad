//! `copad-usage` — the shared subscription rate-limit readout.
//!
//! Lifted out of `copad-cli` (decision #74) once a second consumer needed it:
//! `coctl usage --limits` renders it for a tmux `status-right`, and `comux`
//! polls it in-process for its status bar (so comux no longer shells out to
//! `coctl` and installs standalone). Both providers are read locally — Claude via
//! a live OAuth call, Codex from the newest rollout — with a short-lived on-disk
//! cache bridging Claude's OAuth-token lapses. See [`limits`] for the specifics.

pub mod limits;
