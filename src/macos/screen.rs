use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayIsAsleep, CGGetOnlineDisplayList, CGSessionCopyCurrentDictionary,
};

use crate::macos::window_list::MAX_DISPLAYS;

const SCREEN_IS_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";

pub fn screen_locked() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let session = unsafe { session.cast_unchecked::<CFType, CFType>() };
    locked_in(session)
}

fn locked_in(session: &CFDictionary<CFType, CFType>) -> bool {
    let key = CFString::from_static_str(SCREEN_IS_LOCKED_KEY);
    let Some(locked) = session.get(&key) else {
        return false;
    };
    let Some(locked) = locked.downcast_ref::<CFBoolean>() else {
        return false;
    };
    locked.value()
}

pub fn displays_asleep() -> bool {
    all_asleep(&display_sleep_states())
}

fn display_sleep_states() -> Vec<bool> {
    let mut ids: [CGDirectDisplayID; MAX_DISPLAYS as usize] = [0; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;
    let status = unsafe { CGGetOnlineDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if status.0 != 0 {
        tracing::warn!(status = status.0, "could not read the online display list");
        return Vec::new();
    }

    let mut states = Vec::with_capacity(count as usize);
    for id in ids.iter().take(count as usize) {
        states.push(CGDisplayIsAsleep(*id));
    }
    states
}

fn all_asleep(states: &[bool]) -> bool {
    if states.is_empty() {
        return false;
    }
    for asleep in states {
        if !asleep {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CFRetained;

    use super::*;

    fn session_of(entries: &[(&str, &CFType)]) -> CFRetained<CFDictionary<CFType, CFType>> {
        let names: Vec<CFRetained<CFString>> = entries
            .iter()
            .map(|(name, _)| CFString::from_str(name))
            .collect();
        let keys: Vec<&CFType> = names.iter().map(|name| name as &CFType).collect();
        let values: Vec<&CFType> = entries.iter().map(|(_, value)| *value).collect();
        CFDictionary::from_slices(&keys, &values)
    }

    #[test]
    fn the_lock_key_set_to_true_is_a_locked_session() {
        let session = session_of(&[(SCREEN_IS_LOCKED_KEY, CFBoolean::new(true))]);
        assert!(locked_in(&session));
    }

    #[test]
    fn the_lock_key_set_to_false_is_an_unlocked_session() {
        let session = session_of(&[(SCREEN_IS_LOCKED_KEY, CFBoolean::new(false))]);
        assert!(!locked_in(&session));
    }

    #[test]
    fn a_session_without_the_lock_key_is_unlocked() {
        let on_console = session_of(&[("kCGSSessionOnConsoleKey", CFBoolean::new(true))]);
        assert!(!locked_in(&on_console));
        assert!(!locked_in(&session_of(&[])));
    }

    #[test]
    fn a_lock_key_that_is_not_a_boolean_is_unlocked() {
        let spelled_out = CFString::from_static_str("true");
        let session = session_of(&[(SCREEN_IS_LOCKED_KEY, &spelled_out)]);
        assert!(!locked_in(&session));
    }

    #[test]
    #[ignore = "reads the live machine: needs a login session with a display attached"]
    fn the_live_machine_enumerates_its_displays_and_its_session() {
        assert!(
            !display_sleep_states().is_empty(),
            "no display was enumerated, so `display_asleep` could never read true"
        );
        assert!(
            CGSessionCopyCurrentDictionary().is_some(),
            "no session dictionary, so `screen_locked` could never read true"
        );
    }

    #[test]
    fn no_display_at_all_is_not_a_dark_screen() {
        assert!(!all_asleep(&[]));
    }

    #[test]
    fn every_display_asleep_is_a_dark_screen() {
        assert!(all_asleep(&[true]));
        assert!(all_asleep(&[true, true, true]));
    }

    #[test]
    fn one_lit_display_keeps_the_screen_awake() {
        assert!(!all_asleep(&[false]));
        assert!(!all_asleep(&[true, false]));
        assert!(!all_asleep(&[false, true]));
    }
}
