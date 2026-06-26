use helix_event::{events, register_event};
use helix_view::document::Mode;
use helix_view::events::{
    ConfigDidChange, DiagnosticsDidChange, DocumentDidChange, DocumentDidClose, DocumentDidOpen,
    DocumentFocusLost, LanguageServerExited, LanguageServerInitialized, SelectionDidChange,
};

use crate::commands;
use crate::keymap::MappableCommand;

events! {
    OnModeSwitch<'a, 'cx> { old_mode: Mode, new_mode: Mode, cx: &'a mut commands::Context<'cx> }
    PostInsertChar<'a, 'cx> { c: char, cx: &'a mut commands::Context<'cx> }
    PostCommand<'a, 'cx> { command: & 'a MappableCommand, cx: &'a mut commands::Context<'cx> }
}

pub fn register() {
    // Registering an event twice panics (the registry enforces this for
    // soundness). Production registers exactly once, but unit tests may build
    // several throwaway editors in the same process, so guard with a `Once` to
    // make repeated calls a no-op rather than a panic.
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        register_event::<OnModeSwitch>();
        register_event::<PostInsertChar>();
        register_event::<PostCommand>();
        register_event::<DocumentDidOpen>();
        register_event::<DocumentDidChange>();
        register_event::<DocumentDidClose>();
        register_event::<DocumentFocusLost>();
        register_event::<SelectionDidChange>();
        register_event::<DiagnosticsDidChange>();
        register_event::<LanguageServerInitialized>();
        register_event::<LanguageServerExited>();
        register_event::<ConfigDidChange>();
    });
}
