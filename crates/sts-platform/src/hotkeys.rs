//! Global hotkey binding parsing and `global-hotkey` event dispatch.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, Once, OnceLock};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::PlatformError;

/// Application actions triggerable via global keyboard shortcuts (Spec §5.2.A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    /// Activate freeze-frame Square to Search overlay (default `Ctrl+Shift+S`).
    SquareToSearch,
    /// Toggle Continuous Live Translate session (default `Ctrl+Shift+T`).
    ToggleLiveTranslate,
    /// Toggle Live Translate overlay visibility without ending the session (default `Ctrl+Shift+H`).
    ToggleLiveVisibility,
}

impl HotkeyAction {
    /// Returns the stable `xdg-desktop-portal` GlobalShortcuts identifier for this action.
    #[must_use]
    pub const fn portal_id(self) -> &'static str {
        match self {
            Self::SquareToSearch => "square-to-search",
            Self::ToggleLiveTranslate => "toggle-live-translate",
            Self::ToggleLiveVisibility => "toggle-live-visibility",
        }
    }

    /// Returns the human-readable shortcut description for desktop portals.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::SquareToSearch => {
                "Capture region for Square to Search OCR, translation, and visual search"
            }
            Self::ToggleLiveTranslate => "Start or stop Continuous Live Translate",
            Self::ToggleLiveVisibility => "Show or hide Live Translate overlay",
        }
    }

    /// Resolves a `xdg-desktop-portal` GlobalShortcuts identifier back into a [`HotkeyAction`].
    #[must_use]
    pub fn from_portal_id(id: &str) -> Option<Self> {
        match id {
            "square-to-search" => Some(Self::SquareToSearch),
            "toggle-live-translate" => Some(Self::ToggleLiveTranslate),
            "toggle-live-visibility" => Some(Self::ToggleLiveVisibility),
            _ => None,
        }
    }
}

/// Pair associating a [`HotkeyAction`] with its user-configured shortcut string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub action: HotkeyAction,
    pub spec: String,
}

impl HotkeyBinding {
    /// Creates a new hotkey binding.
    #[must_use]
    pub fn new(action: HotkeyAction, spec: impl Into<String>) -> Self {
        Self {
            action,
            spec: spec.into(),
        }
    }

    /// Parses [`Self::spec`] into a [`global_hotkey::hotkey::HotKey`].
    pub fn parse(&self) -> Result<HotKey, PlatformError> {
        parse_hotkey_binding(&self.spec)
    }

    /// Returns the stable `xdg-desktop-portal` GlobalShortcuts identifier for `action`.
    #[must_use]
    pub const fn portal_id(action: HotkeyAction) -> &'static str {
        action.portal_id()
    }

    /// Returns the human-readable shortcut description for `action`.
    #[must_use]
    pub const fn description(action: HotkeyAction) -> &'static str {
        action.description()
    }
}

fn parse_modifier(token: &str) -> Option<Modifiers> {
    match token {
        "CTRL" | "CONTROL" => Some(Modifiers::CONTROL),
        "ALT" => Some(Modifiers::ALT),
        "SHIFT" => Some(Modifiers::SHIFT),
        "SUPER" | "META" | "WIN" | "CMD" | "COMMAND" => Some(Modifiers::META),
        _ => None,
    }
}

fn parse_main_key(token: &str) -> Option<Code> {
    match token {
        "A" | "KEYA" => Some(Code::KeyA),
        "B" | "KEYB" => Some(Code::KeyB),
        "C" | "KEYC" => Some(Code::KeyC),
        "D" | "KEYD" => Some(Code::KeyD),
        "E" | "KEYE" => Some(Code::KeyE),
        "F" | "KEYF" => Some(Code::KeyF),
        "G" | "KEYG" => Some(Code::KeyG),
        "H" | "KEYH" => Some(Code::KeyH),
        "I" | "KEYI" => Some(Code::KeyI),
        "J" | "KEYJ" => Some(Code::KeyJ),
        "K" | "KEYK" => Some(Code::KeyK),
        "L" | "KEYL" => Some(Code::KeyL),
        "M" | "KEYM" => Some(Code::KeyM),
        "N" | "KEYN" => Some(Code::KeyN),
        "O" | "KEYO" => Some(Code::KeyO),
        "P" | "KEYP" => Some(Code::KeyP),
        "Q" | "KEYQ" => Some(Code::KeyQ),
        "R" | "KEYR" => Some(Code::KeyR),
        "S" | "KEYS" => Some(Code::KeyS),
        "T" | "KEYT" => Some(Code::KeyT),
        "U" | "KEYU" => Some(Code::KeyU),
        "V" | "KEYV" => Some(Code::KeyV),
        "W" | "KEYW" => Some(Code::KeyW),
        "X" | "KEYX" => Some(Code::KeyX),
        "Y" | "KEYY" => Some(Code::KeyY),
        "Z" | "KEYZ" => Some(Code::KeyZ),
        "0" | "DIGIT0" => Some(Code::Digit0),
        "1" | "DIGIT1" => Some(Code::Digit1),
        "2" | "DIGIT2" => Some(Code::Digit2),
        "3" | "DIGIT3" => Some(Code::Digit3),
        "4" | "DIGIT4" => Some(Code::Digit4),
        "5" | "DIGIT5" => Some(Code::Digit5),
        "6" | "DIGIT6" => Some(Code::Digit6),
        "7" | "DIGIT7" => Some(Code::Digit7),
        "8" | "DIGIT8" => Some(Code::Digit8),
        "9" | "DIGIT9" => Some(Code::Digit9),
        "F1" => Some(Code::F1),
        "F2" => Some(Code::F2),
        "F3" => Some(Code::F3),
        "F4" => Some(Code::F4),
        "F5" => Some(Code::F5),
        "F6" => Some(Code::F6),
        "F7" => Some(Code::F7),
        "F8" => Some(Code::F8),
        "F9" => Some(Code::F9),
        "F10" => Some(Code::F10),
        "F11" => Some(Code::F11),
        "F12" => Some(Code::F12),
        _ => None,
    }
}

/// Parses a shortcut specification string supporting `Ctrl`/`Alt`/`Shift`/`Super` modifiers and
/// main keys `A..Z`, `0..9`, and `F1..F12` into a [`HotKey`].
pub fn parse_hotkey_binding(spec: &str) -> Result<HotKey, PlatformError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(PlatformError::InvalidHotkey {
            spec: spec.to_owned(),
            reason: String::from("hotkey specification cannot be empty"),
        });
    }

    let mut mods = Modifiers::empty();
    let mut main_key: Option<Code> = None;

    for raw_token in trimmed.split('+') {
        let token = raw_token.trim();
        if token.is_empty() {
            return Err(PlatformError::InvalidHotkey {
                spec: spec.to_owned(),
                reason: String::from("empty token between '+' separators"),
            });
        }

        if main_key.is_some() {
            return Err(PlatformError::InvalidHotkey {
                spec: spec.to_owned(),
                reason: String::from("modifiers must precede a single main key"),
            });
        }

        let upper = token.to_ascii_uppercase();
        if let Some(modifier) = parse_modifier(&upper) {
            if mods.contains(modifier) {
                return Err(PlatformError::InvalidHotkey {
                    spec: spec.to_owned(),
                    reason: format!("duplicate modifier `{token}`"),
                });
            }
            mods |= modifier;
        } else if let Some(code) = parse_main_key(&upper) {
            main_key = Some(code);
        } else {
            return Err(PlatformError::InvalidHotkey {
                spec: spec.to_owned(),
                reason: format!("unsupported modifier or key `{token}`"),
            });
        }
    }

    let Some(code) = main_key else {
        return Err(PlatformError::InvalidHotkey {
            spec: spec.to_owned(),
            reason: String::from("missing main key (expected A-Z, 0-9, or F1-F12)"),
        });
    };

    let modifiers = (!mods.is_empty()).then_some(mods);
    Ok(HotKey::new(modifiers, code))
}

/// Trait for registering and clearing global hotkeys.
pub trait HotkeyManager {
    /// Parses and registers the supplied bindings, replacing any previously registered bindings.
    fn register_bindings(&mut self, bindings: &[HotkeyBinding]) -> Result<(), PlatformError>;

    /// Unregisters all currently active global hotkeys.
    fn unregister_all(&mut self) -> Result<(), PlatformError>;
}

#[derive(Default)]
struct HotkeyDispatchState {
    id_to_action: HashMap<u32, HotkeyAction>,
    tx: Option<UnboundedSender<HotkeyAction>>,
}

static HOTKEY_DISPATCH: OnceLock<Mutex<HotkeyDispatchState>> = OnceLock::new();
static HOTKEY_HANDLER_INSTALLED: Once = Once::new();

fn dispatch_state() -> &'static Mutex<HotkeyDispatchState> {
    HOTKEY_DISPATCH.get_or_init(|| Mutex::new(HotkeyDispatchState::default()))
}

pub(crate) fn dispatch_hotkey_event(
    id_to_action: &HashMap<u32, HotkeyAction>,
    event: GlobalHotKeyEvent,
    tx: &UnboundedSender<HotkeyAction>,
) -> bool {
    if event.state != HotKeyState::Pressed {
        return false;
    }

    let Some(&action) = id_to_action.get(&event.id) else {
        return false;
    };

    if tx.send(action).is_err() {
        debug!("hotkey channel closed before delivering {action:?}");
        return false;
    }

    true
}

fn handle_global_hotkey_event(event: GlobalHotKeyEvent) {
    let Ok(state) = dispatch_state().lock() else {
        warn!("global hotkey dispatch mutex poisoned");
        return;
    };

    if let Some(tx) = &state.tx {
        let _ = dispatch_hotkey_event(&state.id_to_action, event, tx);
    }
}

/// Production [`HotkeyManager`] backed by `global-hotkey`, forwarding key-press events onto a
/// Tokio unbounded channel.
pub struct GlobalHotkeyService {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
}

impl GlobalHotkeyService {
    /// Initializes the OS global hotkey manager and wires pressed hotkey events to `tx`.
    pub fn new(tx: UnboundedSender<HotkeyAction>) -> Result<Self, PlatformError> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|err| PlatformError::HotkeyFailed(err.to_string()))?;

        {
            let mut state = dispatch_state().lock().map_err(|_| {
                PlatformError::HotkeyFailed(String::from("hotkey dispatch mutex poisoned"))
            })?;
            state.id_to_action.clear();
            state.tx = Some(tx);
        }

        HOTKEY_HANDLER_INSTALLED.call_once(|| {
            GlobalHotKeyEvent::set_event_handler(Some(handle_global_hotkey_event));
        });

        Ok(Self {
            manager,
            registered: Vec::new(),
        })
    }

    /// Initializes the service and immediately registers `bindings`.
    pub fn with_bindings(
        tx: UnboundedSender<HotkeyAction>,
        bindings: &[HotkeyBinding],
    ) -> Result<Self, PlatformError> {
        let mut service = Self::new(tx)?;
        service.register_bindings(bindings)?;
        Ok(service)
    }
}

impl HotkeyManager for GlobalHotkeyService {
    fn register_bindings(&mut self, bindings: &[HotkeyBinding]) -> Result<(), PlatformError> {
        let mut parsed = Vec::with_capacity(bindings.len());
        let mut seen_ids = HashSet::with_capacity(bindings.len());

        for binding in bindings {
            let hotkey = binding.parse()?;
            if !seen_ids.insert(hotkey.id()) {
                return Err(PlatformError::InvalidHotkey {
                    spec: binding.spec.clone(),
                    reason: String::from("duplicate hotkey binding"),
                });
            }
            parsed.push((binding.action, hotkey));
        }

        self.unregister_all()?;

        let mut registered = Vec::with_capacity(parsed.len());
        let mut id_to_action = HashMap::with_capacity(parsed.len());

        for (action, hotkey) in parsed {
            if let Err(err) = self.manager.register(hotkey) {
                if !registered.is_empty() {
                    let _ = self.manager.unregister_all(&registered);
                }
                return Err(PlatformError::HotkeyFailed(err.to_string()));
            }
            id_to_action.insert(hotkey.id(), action);
            registered.push(hotkey);
        }

        {
            let mut state = dispatch_state().lock().map_err(|_| {
                PlatformError::HotkeyFailed(String::from("hotkey dispatch mutex poisoned"))
            })?;
            state.id_to_action = id_to_action;
        }

        self.registered = registered;
        Ok(())
    }

    fn unregister_all(&mut self) -> Result<(), PlatformError> {
        if !self.registered.is_empty() {
            self.manager
                .unregister_all(&self.registered)
                .map_err(|err| PlatformError::HotkeyFailed(err.to_string()))?;
            self.registered.clear();
        }

        let mut state = dispatch_state().lock().map_err(|_| {
            PlatformError::HotkeyFailed(String::from("hotkey dispatch mutex poisoned"))
        })?;
        state.id_to_action.clear();

        Ok(())
    }
}

impl Drop for GlobalHotkeyService {
    fn drop(&mut self) {
        if !self.registered.is_empty()
            && let Err(err) = self.manager.unregister_all(&self.registered)
        {
            warn!("failed to unregister global hotkeys during drop: {err}");
        }
        self.registered.clear();

        if let Ok(mut state) = dispatch_state().lock() {
            state.id_to_action.clear();
            state.tx = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_default_hotkeys_and_rejects_invalid_strings() -> Result<(), PlatformError> {
        let s = parse_hotkey_binding("Ctrl+Shift+S")?;
        assert_eq!(s.mods, Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(s.key, Code::KeyS);

        let t = parse_hotkey_binding("Ctrl+Shift+T")?;
        assert_eq!(t.mods, Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(t.key, Code::KeyT);

        let h = parse_hotkey_binding("Ctrl+Shift+H")?;
        assert_eq!(h.mods, Modifiers::CONTROL | Modifiers::SHIFT);
        assert_eq!(h.key, Code::KeyH);

        assert!(matches!(
            parse_hotkey_binding("NotAMod+???"),
            Err(PlatformError::InvalidHotkey { .. })
        ));
        Ok(())
    }

    #[test]
    fn parses_all_supported_modifiers_digits_and_function_keys() -> Result<(), PlatformError> {
        let combo = parse_hotkey_binding("ctrl + alt + shift + super + f12")?;
        assert_eq!(
            combo.mods,
            Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::META
        );
        assert_eq!(combo.key, Code::F12);

        let digit = parse_hotkey_binding("Alt+0")?;
        assert_eq!(digit.mods, Modifiers::ALT);
        assert_eq!(digit.key, Code::Digit0);

        let fn_only = parse_hotkey_binding("F1")?;
        assert_eq!(fn_only.mods, Modifiers::empty());
        assert_eq!(fn_only.key, Code::F1);

        let binding = HotkeyBinding::new(HotkeyAction::SquareToSearch, "Super+9");
        let parsed = binding.parse()?;
        assert_eq!(parsed.mods, Modifiers::META);
        assert_eq!(parsed.key, Code::Digit9);
        Ok(())
    }

    #[test]
    fn rejects_malformed_hotkey_strings() {
        for invalid in [
            "",
            "   ",
            "Ctrl+",
            "+S",
            "Ctrl++S",
            "Ctrl+Ctrl+S",
            "Ctrl+Shift",
            "Ctrl+S+Shift",
            "Ctrl+S+T",
            "Ctrl+F13",
            "Ctrl+F0",
            "Ctrl+Space",
        ] {
            assert!(
                matches!(
                    parse_hotkey_binding(invalid),
                    Err(PlatformError::InvalidHotkey { .. })
                ),
                "expected `{invalid}` to be rejected"
            );
        }
    }

    #[test]
    fn dispatch_hotkey_event_forwards_only_pressed_events() -> Result<(), PlatformError> {
        let hotkey = parse_hotkey_binding("Ctrl+Shift+S")?;
        let mut map = HashMap::new();
        map.insert(hotkey.id(), HotkeyAction::SquareToSearch);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let released = GlobalHotKeyEvent {
            id: hotkey.id(),
            state: HotKeyState::Released,
        };
        assert!(!dispatch_hotkey_event(&map, released, &tx));
        assert!(rx.try_recv().is_err());

        let pressed = GlobalHotKeyEvent {
            id: hotkey.id(),
            state: HotKeyState::Pressed,
        };
        assert!(dispatch_hotkey_event(&map, pressed, &tx));
        assert_eq!(rx.try_recv().ok(), Some(HotkeyAction::SquareToSearch));
        Ok(())
    }

    #[test]
    fn hotkey_binding_portal_ids_and_descriptions_are_distinct_and_non_empty() {
        let actions = [
            HotkeyAction::SquareToSearch,
            HotkeyAction::ToggleLiveTranslate,
            HotkeyAction::ToggleLiveVisibility,
        ];
        let mut ids = HashSet::new();
        for action in actions {
            let id = HotkeyBinding::portal_id(action);
            let desc = HotkeyBinding::description(action);
            assert!(!id.is_empty());
            assert!(!desc.is_empty());
            assert_eq!(HotkeyAction::from_portal_id(id), Some(action));
            assert!(ids.insert(id), "duplicate portal_id: {id}");
        }
        assert_eq!(HotkeyAction::from_portal_id("unknown"), None);
    }
}
