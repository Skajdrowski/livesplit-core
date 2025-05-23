mod ax;
mod carbon;
mod cf;
mod cg;
mod permission;

use self::{
    carbon::{
        LMGetKbdType, TISCopyCurrentKeyboardInputSource, TISCopyCurrentKeyboardLayoutInputSource,
        TISGetInputSourceProperty, UCKeyAction, UCKeyTranslate, UCKeyTranslateBits,
        kTISPropertyUnicodeKeyLayoutData,
    },
    cf::{
        CFDataGetBytePtr, CFMachPortCreateRunLoopSource, CFRelease, CFRunLoopAddSource,
        CFRunLoopContainsSource, CFRunLoopGetCurrent, CFRunLoopRemoveSource, CFRunLoopRun,
        kCFAllocatorDefault, kCFRunLoopDefaultMode,
    },
    cg::{
        CGEventGetFlags, CGEventTapCreate, CGEventTapEnable, EventField, EventFlags, EventMask,
        EventRef, EventTapLocation, EventTapOptions, EventTapPlacement, EventTapProxy, EventType,
    },
};
use crate::{ConsumePreference, Hotkey, KeyCode, Modifiers, Result};
use core::ptr::null_mut;
use std::{
    collections::{HashMap, hash_map::Entry},
    ffi::c_void,
    fmt,
    sync::{Arc, Mutex, mpsc::channel},
    thread,
};

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    CouldntCreateEventTap,
    CouldntCreateRunLoopSource,
    CouldntGetCurrentRunLoop,
    ThreadStoppedUnexpectedly,
}

impl From<Error> for crate::Error {
    fn from(e: Error) -> Self {
        Self::Platform(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CouldntCreateEventTap => "Failed creating the event tap.",
            Self::CouldntCreateRunLoopSource => "Failed creating the run loop source.",
            Self::CouldntGetCurrentRunLoop => "Failed getting the current run loop.",
            Self::ThreadStoppedUnexpectedly => "The background thread stopped unexpectedly.",
        })
    }
}

struct Owned<T>(*mut T);

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0.cast());
        }
    }
}

#[derive(Copy, Clone)]
struct RunLoop(cf::RunLoopRef);

// https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/RunLoopManagement/RunLoopManagement.html
// "The functions in Core Foundation are generally thread-safe and can be called from any thread."
unsafe impl Send for RunLoop {}
unsafe impl Sync for RunLoop {}

struct State {
    hotkeys: Mutex<HashMap<Hotkey, Box<dyn FnMut() + Send + 'static>>>,
}

/// A hook allows you to listen to hotkeys.
pub struct Hook {
    event_loop: RunLoop,
    state: Arc<State>,
}

impl Drop for Hook {
    fn drop(&mut self) {
        unsafe {
            let mode = cf::CFRunLoopCopyCurrentMode(self.event_loop.0);
            if !mode.is_null() {
                cf::CFRelease(mode.cast());
                cf::CFRunLoopStop(self.event_loop.0);
            }
        }
    }
}

impl Hook {
    pub fn new(consume: ConsumePreference) -> Result<Self> {
        let is_consuming = matches!(
            consume,
            ConsumePreference::PreferConsume | ConsumePreference::MustConsume,
        );

        let state = Arc::new(State {
            hotkeys: Mutex::new(HashMap::new()),
        });
        let thread_state = state.clone();

        let (sender, receiver) = channel();

        // The code here is mostly based on:
        // https://github.com/kwhat/libuiohook/blob/f4bb19be8aee7d7ee5ead89b5a89dbf440e2a71a/src/darwin/input_hook.c#L1086

        thread::spawn(move || unsafe {
            let state_ptr: *const State = &*thread_state;

            // If we don't have the permission to use the hotkeys, we want to
            // request it. In that case, this however will fail regardless of
            // whether the permission is then granted by the user or not. The
            // user will have to restart the application. If the permission is
            // already granted, this does nothing.
            permission::request();

            let port = CGEventTapCreate(
                EventTapLocation::SESSION,
                EventTapPlacement::HEAD_INSERT_EVENT_TAP,
                if is_consuming {
                    EventTapOptions::DEFAULT_TAP
                } else {
                    EventTapOptions::LISTEN_ONLY
                },
                EventMask::KEY_DOWN,
                Some(callback),
                state_ptr as *mut c_void,
            );
            if port.is_null() {
                let _ = sender.send(Err(Error::CouldntCreateEventTap));
                return;
            }
            let port = Owned(port);

            let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, port.0, 0);
            if source.is_null() {
                let _ = sender.send(Err(Error::CouldntCreateRunLoopSource));
                return;
            }
            let source = Owned(source);

            let event_loop = CFRunLoopGetCurrent();
            if event_loop.is_null() {
                let _ = sender.send(Err(Error::CouldntGetCurrentRunLoop));
                return;
            }

            CFRunLoopAddSource(event_loop, source.0, kCFRunLoopDefaultMode);

            if { sender }.send(Ok(RunLoop(event_loop))).is_ok() {
                CFRunLoopRun();
            }

            if CFRunLoopContainsSource(event_loop, source.0, kCFRunLoopDefaultMode) {
                CFRunLoopRemoveSource(event_loop, source.0, kCFRunLoopDefaultMode);
            }
            CGEventTapEnable(port.0, false);
        });

        let event_loop = receiver
            .recv()
            .map_err(|_| Error::ThreadStoppedUnexpectedly)??;

        Ok(Hook { event_loop, state })
    }

    pub fn register<F>(&self, hotkey: Hotkey, callback: F) -> Result<()>
    where
        F: FnMut() + Send + 'static,
    {
        if let Entry::Vacant(vacant) = self.state.hotkeys.lock().unwrap().entry(hotkey) {
            vacant.insert(Box::new(callback));
            Ok(())
        } else {
            Err(crate::Error::AlreadyRegistered)
        }
    }

    pub fn unregister(&self, hotkey: Hotkey) -> Result<()> {
        let _ = self
            .state
            .hotkeys
            .lock()
            .unwrap()
            .remove(&hotkey)
            .ok_or(crate::Error::NotRegistered)?;
        Ok(())
    }

    pub fn try_resolve(&self, key_code: KeyCode) -> Option<String> {
        unsafe {
            let current_keyboard_raw = TISCopyCurrentKeyboardInputSource();
            if current_keyboard_raw.is_null() {
                return None;
            }
            let mut current_keyboard = Owned(current_keyboard_raw);

            let mut layout_data =
                TISGetInputSourceProperty(current_keyboard.0, kTISPropertyUnicodeKeyLayoutData);

            if layout_data.is_null() {
                let current_keyboard_raw = TISCopyCurrentKeyboardLayoutInputSource();
                if current_keyboard_raw.is_null() {
                    return None;
                }
                current_keyboard = Owned(current_keyboard_raw);

                layout_data =
                    TISGetInputSourceProperty(current_keyboard.0, kTISPropertyUnicodeKeyLayoutData);
                if layout_data.is_null() {
                    return None;
                }
            }

            let keyboard_layout = CFDataGetBytePtr(layout_data.cast());

            let key_code = match key_code {
                KeyCode::Backquote => 0x32,
                KeyCode::Backslash => 0x2A,
                KeyCode::BracketLeft => 0x21,
                KeyCode::BracketRight => 0x1E,
                KeyCode::Comma => 0x2B,
                KeyCode::Digit0 => 0x1D,
                KeyCode::Digit1 => 0x12,
                KeyCode::Digit2 => 0x13,
                KeyCode::Digit3 => 0x14,
                KeyCode::Digit4 => 0x15,
                KeyCode::Digit5 => 0x17,
                KeyCode::Digit6 => 0x16,
                KeyCode::Digit7 => 0x1A,
                KeyCode::Digit8 => 0x1C,
                KeyCode::Digit9 => 0x19,
                KeyCode::Equal => 0x18,
                KeyCode::IntlBackslash => 0x0A,
                KeyCode::IntlRo => 0x5E,
                KeyCode::IntlYen => 0x5D,
                KeyCode::KeyA => 0x00,
                KeyCode::KeyB => 0x0B,
                KeyCode::KeyC => 0x08,
                KeyCode::KeyD => 0x02,
                KeyCode::KeyE => 0x0E,
                KeyCode::KeyF => 0x03,
                KeyCode::KeyG => 0x05,
                KeyCode::KeyH => 0x04,
                KeyCode::KeyI => 0x22,
                KeyCode::KeyJ => 0x26,
                KeyCode::KeyK => 0x28,
                KeyCode::KeyL => 0x25,
                KeyCode::KeyM => 0x2E,
                KeyCode::KeyN => 0x2D,
                KeyCode::KeyO => 0x1F,
                KeyCode::KeyP => 0x23,
                KeyCode::KeyQ => 0x0C,
                KeyCode::KeyR => 0x0F,
                KeyCode::KeyS => 0x01,
                KeyCode::KeyT => 0x11,
                KeyCode::KeyU => 0x20,
                KeyCode::KeyV => 0x09,
                KeyCode::KeyW => 0x0D,
                KeyCode::KeyX => 0x07,
                KeyCode::KeyY => 0x10,
                KeyCode::KeyZ => 0x06,
                KeyCode::Minus => 0x1B,
                KeyCode::Period => 0x2F,
                KeyCode::Quote => 0x27,
                KeyCode::Semicolon => 0x29,
                KeyCode::Slash => 0x2C,
                _ => return None,
            };

            let mut chars = [0; 4];
            let mut len = 0;

            UCKeyTranslate(
                keyboard_layout.cast(),
                key_code,
                UCKeyAction::Display as _,
                0,
                LMGetKbdType() as _,
                UCKeyTranslateBits::NO_DEAD_KEYS_BIT.bits(),
                &mut 0,
                4,
                &mut len,
                chars.as_mut_ptr(),
            );

            if len == 0 {
                return None;
            }

            String::from_utf16(&chars[..len as usize]).ok()
        }
    }
}

unsafe extern "C" fn callback(
    _: EventTapProxy,
    _: EventType,
    event: EventRef,
    user_info: *mut c_void,
) -> EventRef {
    // If the tap ever gets disabled by a timeout, we may need the following code:
    // // Handle the timeout case by re-enabling the tap.
    // if (type == kCGEventTapDisabledByTimeout) {
    //   CGEventTapEnable(shortcut_listener->event_tap_, TRUE);
    //   return event;
    // }

    let is_repeating =
        unsafe { cg::CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_AUTOREPEAT) };
    if is_repeating != 0 {
        return event;
    }

    let key_code =
        unsafe { cg::CGEventGetIntegerValueField(event, EventField::KEYBOARD_EVENT_KEYCODE) };
    let key_code = match key_code {
        0x00 => KeyCode::KeyA,
        0x01 => KeyCode::KeyS,
        0x02 => KeyCode::KeyD,
        0x03 => KeyCode::KeyF,
        0x04 => KeyCode::KeyH,
        0x05 => KeyCode::KeyG,
        0x06 => KeyCode::KeyZ,
        0x07 => KeyCode::KeyX,
        0x08 => KeyCode::KeyC,
        0x09 => KeyCode::KeyV,
        0x0A => KeyCode::IntlBackslash,
        0x0B => KeyCode::KeyB,
        0x0C => KeyCode::KeyQ,
        0x0D => KeyCode::KeyW,
        0x0E => KeyCode::KeyE,
        0x0F => KeyCode::KeyR,
        0x10 => KeyCode::KeyY,
        0x11 => KeyCode::KeyT,
        0x12 => KeyCode::Digit1,
        0x13 => KeyCode::Digit2,
        0x14 => KeyCode::Digit3,
        0x15 => KeyCode::Digit4,
        0x16 => KeyCode::Digit6,
        0x17 => KeyCode::Digit5,
        0x18 => KeyCode::Equal,
        0x19 => KeyCode::Digit9,
        0x1A => KeyCode::Digit7,
        0x1B => KeyCode::Minus,
        0x1C => KeyCode::Digit8,
        0x1D => KeyCode::Digit0,
        0x1E => KeyCode::BracketRight,
        0x1F => KeyCode::KeyO,
        0x20 => KeyCode::KeyU,
        0x21 => KeyCode::BracketLeft,
        0x22 => KeyCode::KeyI,
        0x23 => KeyCode::KeyP,
        0x24 => KeyCode::Enter,
        0x25 => KeyCode::KeyL,
        0x26 => KeyCode::KeyJ,
        0x27 => KeyCode::Quote,
        0x28 => KeyCode::KeyK,
        0x29 => KeyCode::Semicolon,
        0x2A => KeyCode::Backslash,
        0x2B => KeyCode::Comma,
        0x2C => KeyCode::Slash,
        0x2D => KeyCode::KeyN,
        0x2E => KeyCode::KeyM,
        0x2F => KeyCode::Period,
        0x30 => KeyCode::Tab,
        0x31 => KeyCode::Space,
        0x32 => KeyCode::Backquote,
        0x33 => KeyCode::Backspace,
        0x34 => KeyCode::NumpadEnter, // Not Chrome
        0x35 => KeyCode::Escape,
        0x36 => KeyCode::MetaRight,
        0x37 => KeyCode::MetaLeft,
        0x38 => KeyCode::ShiftLeft,
        0x39 => KeyCode::CapsLock,
        0x3A => KeyCode::AltLeft,
        0x3B => KeyCode::ControlLeft,
        0x3C => KeyCode::ShiftRight,
        0x3D => KeyCode::AltRight,
        0x3E => KeyCode::ControlRight,
        0x3F => KeyCode::Fn, // Not Chrome
        0x40 => KeyCode::F17,
        0x41 => KeyCode::NumpadDecimal,
        0x43 => KeyCode::NumpadMultiply,
        0x45 => KeyCode::NumpadAdd,
        0x47 => KeyCode::NumLock,
        0x48 => KeyCode::AudioVolumeUp,
        0x49 => KeyCode::AudioVolumeDown,
        0x4A => KeyCode::AudioVolumeMute,
        0x4B => KeyCode::NumpadDivide,
        0x4C => KeyCode::NumpadEnter,
        0x4E => KeyCode::NumpadSubtract,
        0x4F => KeyCode::F18,
        0x50 => KeyCode::F19,
        0x51 => KeyCode::NumpadEqual,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::Numpad1,
        0x54 => KeyCode::Numpad2,
        0x55 => KeyCode::Numpad3,
        0x56 => KeyCode::Numpad4,
        0x57 => KeyCode::Numpad5,
        0x58 => KeyCode::Numpad6,
        0x59 => KeyCode::Numpad7,
        0x5A => KeyCode::F20,
        0x5B => KeyCode::Numpad8,
        0x5C => KeyCode::Numpad9,
        0x5D => KeyCode::IntlYen,
        0x5E => KeyCode::IntlRo,
        0x5F => KeyCode::NumpadComma,
        0x60 => KeyCode::F5,
        0x61 => KeyCode::F6,
        0x62 => KeyCode::F7,
        0x63 => KeyCode::F3,
        0x64 => KeyCode::F8,
        0x65 => KeyCode::F9,
        0x66 => KeyCode::Lang2,
        0x67 => KeyCode::F11,
        0x68 => KeyCode::Lang1, // KanaMode in Safari
        0x69 => KeyCode::F13,
        0x6A => KeyCode::F16,
        0x6B => KeyCode::F14,
        0x6D => KeyCode::F10,
        0x6E => KeyCode::ContextMenu, // Missing on MDN
        0x6F => KeyCode::F12,
        0x71 => KeyCode::F15,
        // Apple hasn't been producing any keyboards with `Help` anymore since
        // 2007. So this can be considered Insert instead.
        0x72 => KeyCode::Insert,
        0x73 => KeyCode::Home,
        0x74 => KeyCode::PageUp,
        0x75 => KeyCode::Delete,
        0x76 => KeyCode::F4,
        0x77 => KeyCode::End,
        0x78 => KeyCode::F2,
        0x79 => KeyCode::PageDown,
        0x7A => KeyCode::F1,
        0x7B => KeyCode::ArrowLeft,
        0x7C => KeyCode::ArrowRight,
        0x7D => KeyCode::ArrowDown,
        0x7E => KeyCode::ArrowUp,
        _ => return event,
    };

    let state = user_info as *const State;
    let state = unsafe { &*state };

    let modifier_flags = unsafe { CGEventGetFlags(event) };
    let mut modifiers = Modifiers::empty();

    if modifier_flags.contains(EventFlags::SHIFT)
        && !matches!(key_code, KeyCode::ShiftLeft | KeyCode::ShiftRight)
    {
        modifiers.insert(Modifiers::SHIFT);
    }

    if modifier_flags.contains(EventFlags::CONTROL)
        && !matches!(key_code, KeyCode::ControlLeft | KeyCode::ControlRight)
    {
        modifiers.insert(Modifiers::CONTROL);
    }

    if modifier_flags.contains(EventFlags::OPTION)
        && !matches!(key_code, KeyCode::AltLeft | KeyCode::AltRight)
    {
        modifiers.insert(Modifiers::ALT);
    }

    if modifier_flags.contains(EventFlags::COMMAND)
        && !matches!(key_code, KeyCode::MetaLeft | KeyCode::MetaRight)
    {
        modifiers.insert(Modifiers::META);
    }

    if let Some(callback) = state
        .hotkeys
        .lock()
        .unwrap()
        .get_mut(&key_code.with_modifiers(modifiers))
    {
        callback();

        // If we handled the event and the hook is consuming, we should return
        // null so the system deletes the event. If the hook is not consuming
        // the return value will be ignored, so return null anyway.
        null_mut()
    } else {
        event
    }
}
