//! GDK key events → the kitty keyboard protocol, for terminals behind a [`Relay`]
//! (decisions #164).
//!
//! An `EventControllerKey` in the CAPTURE phase sees each key before VTE does. While the
//! program negotiated no flags it does nothing at all; once it has, keys that the protocol
//! encodes are sent here and stopped, and keys that simply type text go on to VTE (and its
//! input method) untouched.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use copad_keys::{Action, Encoded, Key, KeyEvent, Modifier, mods};
use gtk4::gdk;
use gtk4::glib;
use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use vte4::prelude::*;

use crate::kitty_relay::Relay;

#[derive(Default)]
struct Keys {
    /// Hardware keycodes held down — a press of one already here is a repeat.
    held: HashSet<u32>,
    /// Keycodes whose press went out encoded; only these report a release.
    encoded: HashSet<u32>,
    /// Modifier keys held, so releasing one Shift while the other is down keeps the bit.
    held_mods: HashSet<Modifier>,
}

/// Attach the key handling to `terminal`. `relay` holds the negotiated flags.
pub fn attach(terminal: &vte4::Terminal, relay: Rc<RefCell<Option<Relay>>>) {
    let keys = Rc::new(RefCell::new(Keys::default()));
    let controller = gtk4::EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let (term, relay, keys) = (terminal.clone(), relay.clone(), keys.clone());
        controller.connect_key_pressed(move |c, keyval, keycode, state| {
            let held = keys.borrow().held.contains(&keycode);
            keys.borrow_mut().held.insert(keycode);
            let action = if held { Action::Repeat } else { Action::Press };
            let event = c
                .current_event()
                .and_then(|e| e.downcast::<gdk::KeyEvent>().ok());
            handle(
                &term,
                &relay,
                &keys,
                event.as_ref(),
                (keyval, keycode, state),
                action,
            )
        });
    }
    {
        let (term, relay, keys) = (terminal.clone(), relay.clone(), keys.clone());
        controller.connect_key_released(move |c, keyval, keycode, state| {
            keys.borrow_mut().held.remove(&keycode);
            if !keys.borrow_mut().encoded.remove(&keycode) {
                return;
            }
            let event = c
                .current_event()
                .and_then(|e| e.downcast::<gdk::KeyEvent>().ok());
            handle(
                &term,
                &relay,
                &keys,
                event.as_ref(),
                (keyval, keycode, state),
                Action::Release,
            );
        });
    }
    terminal.add_controller(controller);

    let focus = gtk4::EventControllerFocus::new();
    focus.connect_leave(move |_| {
        let mut k = keys.borrow_mut();
        k.held.clear();
        k.encoded.clear();
        k.held_mods.clear();
    });
    terminal.add_controller(focus);
}

/// copad's own terminal chords (font zoom) stay copad's.
fn is_host_chord(keyval: gdk::Key, state: gdk::ModifierType) -> bool {
    state.contains(gdk::ModifierType::CONTROL_MASK)
        && matches!(
            keyval,
            gdk::Key::equal | gdk::Key::plus | gdk::Key::minus | gdk::Key::_0
        )
}

fn handle(
    term: &vte4::Terminal,
    relay: &Rc<RefCell<Option<Relay>>>,
    keys: &Rc<RefCell<Keys>>,
    event: Option<&gdk::KeyEvent>,
    (keyval, keycode, state): (gdk::Key, u32, gdk::ModifierType),
    action: Action,
) -> glib::Propagation {
    let (flags, app_cursor) = match relay.borrow().as_ref() {
        Some(r) => (r.flags(), r.app_cursor()),
        None => return glib::Propagation::Proceed,
    };
    if flags == 0 || (action != Action::Release && is_host_chord(keyval, state)) {
        return glib::Propagation::Proceed;
    }
    let Some(ev) = key_event(term, event, keyval, keycode, state, action, keys) else {
        return glib::Propagation::Proceed;
    };
    let text = ev.1;
    let ev = KeyEvent {
        text: &text,
        ..ev.0
    };
    match copad_keys::encode(&ev, flags, app_cursor) {
        Encoded::Text => glib::Propagation::Proceed,
        Encoded::Nothing => {
            // A release nobody asked for, or a lone modifier: VTE has nothing to do either.
            if action == Action::Release {
                glib::Propagation::Proceed
            } else {
                glib::Propagation::Stop
            }
        }
        Encoded::Bytes(bytes) => {
            if action != Action::Release {
                keys.borrow_mut().encoded.insert(keycode);
                if term.is_scroll_on_keystroke()
                    && let Some(adj) = term.vadjustment()
                {
                    adj.set_value(adj.upper() - adj.page_size());
                }
            }
            term.feed_child(&bytes);
            glib::Propagation::Stop
        }
    }
}

/// The kitty modifier bits for a GDK state. With the stock XKB rules GDK reports Meta along
/// with Alt and Hyper along with Super; those only count when they come alone.
fn kitty_mods(state: gdk::ModifierType) -> u8 {
    use gdk::ModifierType as M;
    let mut m = 0;
    let has = |f| state.contains(f);
    if has(M::SHIFT_MASK) {
        m |= mods::SHIFT;
    }
    if has(M::ALT_MASK) {
        m |= mods::ALT;
    }
    if has(M::CONTROL_MASK) {
        m |= mods::CTRL;
    }
    if has(M::SUPER_MASK) {
        m |= mods::SUPER;
    }
    if has(M::HYPER_MASK) && !has(M::SUPER_MASK) {
        m |= mods::HYPER;
    }
    if has(M::META_MASK) && !has(M::ALT_MASK) {
        m |= mods::META;
    }
    if has(M::LOCK_MASK) {
        m |= mods::CAPS_LOCK;
    }
    m
}

/// Build the protocol's view of a GDK key event (the text is returned beside it).
fn key_event(
    term: &vte4::Terminal,
    event: Option<&gdk::KeyEvent>,
    keyval: gdk::Key,
    keycode: u32,
    state: gdk::ModifierType,
    action: Action,
    keys: &Rc<RefCell<Keys>>,
) -> Option<(KeyEvent<'static>, String)> {
    let display = term.display();
    let mut m = kitty_mods(state);
    if display
        .default_seat()
        .and_then(|s| s.keyboard())
        .is_some_and(|k| k.is_num_locked())
    {
        m |= mods::NUM_LOCK;
    }
    let base = copad_keys::layout::us_base_evdev(keycode.saturating_sub(8));
    if let Some(key) = copad_keys::layout::xkb_functional(keyval.into_glib()) {
        if let Key::Modifier(modifier) = key {
            // The event's own effect on its modifier: set on press, cleared on release
            // unless the same modifier on the other side is still down.
            let mut k = keys.borrow_mut();
            if action == Action::Release {
                k.held_mods.remove(&modifier);
                let twin_held = k.held_mods.iter().any(|h| h.bit() == modifier.bit());
                if !twin_held {
                    m &= !modifier.bit();
                }
            } else {
                k.held_mods.insert(modifier);
                m |= modifier.bit();
            }
        }
        // Keypad digits and operators type text while Num Lock is on.
        let text = match key {
            Key::Kp(_) => keyval.to_unicode().filter(|c| !c.is_control()),
            _ => None,
        }
        .map(String::from)
        .unwrap_or_default();
        let ev = KeyEvent {
            key,
            shifted: None,
            base: None,
            mods: m,
            action,
            text: "",
        };
        return Some((ev, text));
    }

    // A text key: name it by what it types unshifted in the active layout (level 0 of the
    // event's layout), with what it types shifted (level 1) alongside.
    let layout = event.map(|e| e.layout());
    let level_char = |level: u32| -> Option<char> {
        let entries = display.map_keycode(keycode)?;
        entries
            .iter()
            .find(|(k, _)| Some(k.group() as u32) == layout && k.level() as u32 == level)
            .or_else(|| entries.iter().find(|(k, _)| k.level() as u32 == level))
            .and_then(|(_, sym)| sym.to_unicode())
            .filter(|c| !c.is_control())
    };
    let unshifted = level_char(0)
        .or_else(|| keyval.to_lower().to_unicode())
        .or(base)?;
    let key = {
        let mut lower = unshifted.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(l), None) => l,
            _ => unshifted,
        }
    };
    // Ctrl / Alt / Super turn the key into a chord unless the layout consumed them (AltGr).
    let consumed = event
        .map(|e| e.consumed_modifiers())
        .unwrap_or(gdk::ModifierType::empty());
    let chord_mods = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK;
    let chord = (state - consumed).intersects(chord_mods);
    let text = if chord {
        String::new()
    } else {
        keyval
            .to_unicode()
            .filter(|c| !c.is_control())
            .map(String::from)
            .unwrap_or_default()
    };
    let ev = KeyEvent {
        key: Key::Char(key),
        shifted: level_char(1),
        base,
        mods: m,
        action,
        text: "",
    };
    Some((ev, text))
}
