use std::collections::BTreeSet;
use std::sync::Mutex;

use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use crate::keymap::scancode_to_keycode;

/// macOS keycodes with special handling
const KEY_CAPS_LOCK: CGKeyCode = 0x39;
const KEY_FUNCTION: CGKeyCode = 0x3F;

/// The modifier flag a modifier key sets, if it is one
fn modifier_flag(keycode: CGKeyCode) -> Option<CGEventFlags> {
    match keycode {
        0x38 | 0x3C => Some(CGEventFlags::CGEventFlagShift),
        0x3B | 0x3E => Some(CGEventFlags::CGEventFlagControl),
        0x3A | 0x3D => Some(CGEventFlags::CGEventFlagAlternate),
        0x37 | 0x36 => Some(CGEventFlags::CGEventFlagCommand),
        _ => None,
    }
}

#[derive(Default)]
struct KeyState {
    /// macOS keycodes we've pressed and not yet released
    held: BTreeSet<CGKeyCode>,
    caps_lock: bool,
}

impl KeyState {
    /// Modifier flags implied by the keys we're holding.
    ///
    /// Every event gets exactly these. Left to itself, CGEvent copies the
    /// system's current flags, and macOS adds Fn to synthesized arrow, F- and
    /// navigation keys; that Fn then sticks, turning "e" into Globe+E (emoji
    /// picker) and "d" into Globe+D (Dictation).
    fn flags(&self) -> CGEventFlags {
        let mut flags = self
            .held
            .iter()
            .filter_map(|&k| modifier_flag(k))
            .fold(CGEventFlags::CGEventFlagNull, |acc, f| acc | f);
        if self.caps_lock {
            flags |= CGEventFlags::CGEventFlagAlphaShift;
        }
        flags
    }
}

pub struct KeyboardInjector {
    state: Mutex<KeyState>,
}

impl KeyboardInjector {
    pub fn new() -> Result<Self> {
        // Verify we can create an event source (permission check)
        let _ = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource — check Accessibility permission"))?;
        let injector = Self { state: Mutex::new(KeyState::default()) };
        // A previous session may have left Fn stuck down
        injector.clear_fn(&injector.state.lock().unwrap());
        Ok(injector)
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow::anyhow!("Failed to create CGEventSource"))
    }

    /// Inject a key press or release event
    pub fn inject_key(&self, scancode: u8, extended: bool, pressed: bool) -> Result<()> {
        let keycode = match scancode_to_keycode(scancode, extended) {
            Some(kc) => kc as CGKeyCode,
            None => {
                tracing::warn!(scancode, extended, "Unknown scancode, ignoring");
                return Ok(());
            }
        };

        let mut state = self.state.lock().unwrap();
        if pressed {
            state.held.insert(keycode);
        } else if !state.held.remove(&keycode) {
            // Clients release every modifier when their window gains focus, even
            // keys that were never pressed. Passing those on makes macOS see e.g.
            // "Control pressed twice" (the Dictation shortcut), so drop them.
            tracing::trace!(scancode, keycode, "Ignoring release of a key that isn't held");
            return Ok(());
        }

        if keycode == KEY_CAPS_LOCK {
            // Caps Lock toggles on press; a real keyboard sends nothing on release
            if pressed {
                state.caps_lock = !state.caps_lock;
                Self::post(keycode, true, CGEventType::FlagsChanged, state.flags())?;
            }
        } else if modifier_flag(keycode).is_some() {
            Self::post(keycode, pressed, CGEventType::FlagsChanged, state.flags())?;
        } else {
            let event_type = if pressed { CGEventType::KeyDown } else { CGEventType::KeyUp };
            Self::post(keycode, pressed, event_type, state.flags())?;
        }
        tracing::trace!(scancode, keycode, pressed, "Keyboard event injected");
        Ok(())
    }

    /// Releases every key we're holding, e.g. when the client resyncs or leaves,
    /// so no modifier stays stuck down on the Mac.
    pub fn release_all(&self) {
        let mut state = self.state.lock().unwrap();
        let held = std::mem::take(&mut state.held);
        for keycode in held {
            if keycode == KEY_CAPS_LOCK {
                continue;
            }
            let event_type = if modifier_flag(keycode).is_some() {
                CGEventType::FlagsChanged
            } else {
                CGEventType::KeyUp
            };
            // Recompute flags as if this key were already up
            if let Err(e) = Self::post(keycode, false, event_type, state.flags()) {
                tracing::warn!(keycode, "Failed to release key: {e}");
            }
        }
        self.clear_fn(&state);
    }

    /// Posts an Fn-up so macOS stops treating Fn/Globe as held.
    fn clear_fn(&self, state: &KeyState) {
        if let Err(e) = Self::post(KEY_FUNCTION, false, CGEventType::FlagsChanged, state.flags()) {
            tracing::warn!("Failed to clear Fn state: {e}");
        }
    }

    fn post(keycode: CGKeyCode, pressed: bool, event_type: CGEventType, flags: CGEventFlags) -> Result<()> {
        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(source, keycode, pressed)
            .map_err(|_| anyhow::anyhow!("Failed to create keyboard event"))?;
        event.set_type(event_type);
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Inject a unicode character press/release
    pub fn inject_unicode(&self, ch: u16, pressed: bool) -> Result<()> {
        let source = Self::source()?;
        let event = CGEvent::new_keyboard_event(source, 0, pressed)
            .map_err(|_| anyhow::anyhow!("Failed to create unicode event"))?;

        if pressed {
            event.set_string_from_utf16_unchecked(&[ch]);
        }
        // The character is already final; don't let held/stuck modifiers alter it
        event.set_flags(CGEventFlags::CGEventFlagNull);

        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl Drop for KeyboardInjector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_follow_held_modifiers() {
        let mut state = KeyState::default();
        assert_eq!(state.flags(), CGEventFlags::CGEventFlagNull);
        state.held.insert(0x38); // left shift
        state.held.insert(0x00); // 'a' adds nothing
        assert_eq!(state.flags(), CGEventFlags::CGEventFlagShift);
        state.held.insert(0x3E); // right control
        state.caps_lock = true;
        assert_eq!(
            state.flags(),
            CGEventFlags::CGEventFlagShift | CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlphaShift
        );
        assert!(!state.flags().contains(CGEventFlags::CGEventFlagSecondaryFn));
    }
}
