use objc2_core_foundation::{CFBoolean, CFString, CFType};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayIsAsleep, CGGetActiveDisplayList, CGSessionCopyCurrentDictionary,
};

const MAX_DISPLAYS: u32 = 32;
const SCREEN_IS_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";

pub fn screen_locked() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let session = unsafe { session.cast_unchecked::<CFType, CFType>() };
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
    let status = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if status.0 != 0 {
        tracing::warn!(status = status.0, "could not read the active display list");
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
    use super::*;

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
