use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioHardwarePropertyDefaultInputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_graphics::{CGEvent, CGEventSource, CGEventSourceStateID, CGEventType};

use super::window_list::DisplayEntry;

const ANY_INPUT_EVENT_TYPE: CGEventType = CGEventType(u32::MAX);
const SESSION_STATE: CGEventSourceStateID = CGEventSourceStateID::CombinedSessionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputCounters {
    pub keys: u32,
    pub mouse: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputDelta {
    pub keys: u32,
    pub mouse: u32,
}

#[derive(Debug, Default)]
pub struct InputCounterTracker {
    previous: Option<InputCounters>,
}

impl InputCounterTracker {
    pub fn new() -> Self {
        Self { previous: None }
    }

    pub fn advance(&mut self, current: InputCounters) -> InputDelta {
        let Some(previous) = self.previous.replace(current) else {
            return InputDelta::default();
        };
        InputDelta {
            keys: current.keys.wrapping_sub(previous.keys),
            mouse: current.mouse.wrapping_sub(previous.mouse),
        }
    }
}

pub fn idle_seconds() -> i64 {
    truncate_idle(CGEventSource::seconds_since_last_event_type(
        SESSION_STATE,
        ANY_INPUT_EVENT_TYPE,
    ))
}

pub fn input_counters() -> InputCounters {
    let keys = CGEventSource::counter_for_event_type(SESSION_STATE, CGEventType::KeyDown);
    let mut mouse = 0u32;
    for event_type in [
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
        CGEventType::ScrollWheel,
    ] {
        mouse = mouse.wrapping_add(CGEventSource::counter_for_event_type(
            SESSION_STATE,
            event_type,
        ));
    }
    InputCounters { keys, mouse }
}

pub fn microphone_active() -> bool {
    let Some(device) = default_input_device() else {
        return false;
    };
    let Some(running) = audio_property_u32(device, kAudioDevicePropertyDeviceIsRunningSomewhere)
    else {
        return false;
    };
    running != 0
}

pub fn cursor_display_index(displays: &[DisplayEntry]) -> Option<usize> {
    let event = CGEvent::new(None)?;
    let location = CGEvent::location(Some(&event));
    display_index_at(location.x, location.y, displays)
}

pub fn display_index_at(x: f64, y: f64, displays: &[DisplayEntry]) -> Option<usize> {
    for display in displays {
        let DisplayEntry { index, bounds } = display;
        if bounds.contains(x, y) {
            return Some(*index);
        }
    }
    None
}

fn truncate_idle(seconds: f64) -> i64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    seconds.trunc() as i64
}

fn default_input_device() -> Option<AudioObjectID> {
    let device = audio_property_u32(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultInputDevice,
    )?;
    if device == 0 {
        return None;
    }
    Some(device)
}

fn audio_property_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        tracing::debug!(status, selector, "could not read the audio property");
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::window_list::Rect;

    fn displays() -> Vec<DisplayEntry> {
        vec![
            DisplayEntry {
                index: 0,
                bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            },
            DisplayEntry {
                index: 1,
                bounds: Rect::new(-2560.0, -300.0, 2560.0, 1440.0),
            },
        ]
    }

    #[test]
    fn the_first_sample_reports_no_delta() {
        let mut tracker = InputCounterTracker::new();
        let delta = tracker.advance(InputCounters {
            keys: 900_000,
            mouse: 40_000,
        });
        assert_eq!(delta, InputDelta { keys: 0, mouse: 0 });
    }

    #[test]
    fn a_later_sample_reports_the_difference() {
        let mut tracker = InputCounterTracker::new();
        tracker.advance(InputCounters {
            keys: 900_000,
            mouse: 40_000,
        });
        let delta = tracker.advance(InputCounters {
            keys: 900_184,
            mouse: 40_022,
        });
        assert_eq!(
            delta,
            InputDelta {
                keys: 184,
                mouse: 22
            }
        );
    }

    #[test]
    fn a_counter_that_wraps_still_reports_the_real_delta() {
        let mut tracker = InputCounterTracker::new();
        tracker.advance(InputCounters {
            keys: u32::MAX - 3,
            mouse: u32::MAX,
        });
        let delta = tracker.advance(InputCounters { keys: 6, mouse: 4 });
        assert_eq!(delta, InputDelta { keys: 10, mouse: 5 });
    }

    #[test]
    fn an_unchanged_counter_reports_zero() {
        let mut tracker = InputCounterTracker::new();
        tracker.advance(InputCounters {
            keys: 12,
            mouse: 34,
        });
        let delta = tracker.advance(InputCounters {
            keys: 12,
            mouse: 34,
        });
        assert_eq!(delta, InputDelta { keys: 0, mouse: 0 });
    }

    #[test]
    fn idle_seconds_are_truncated_toward_zero() {
        assert_eq!(truncate_idle(3.9), 3);
        assert_eq!(truncate_idle(0.4), 0);
        assert_eq!(truncate_idle(0.0), 0);
        assert_eq!(truncate_idle(1799.999), 1799);
    }

    #[test]
    fn a_nonsensical_idle_reading_becomes_zero() {
        assert_eq!(truncate_idle(-1.0), 0);
        assert_eq!(truncate_idle(f64::NAN), 0);
        assert_eq!(truncate_idle(f64::INFINITY), 0);
    }

    #[test]
    fn the_cursor_resolves_to_the_display_containing_it() {
        assert_eq!(display_index_at(10.0, 10.0, &displays()), Some(0));
        assert_eq!(display_index_at(-2000.0, 100.0, &displays()), Some(1));
    }

    #[test]
    fn a_cursor_outside_every_display_resolves_to_nothing() {
        assert_eq!(display_index_at(9000.0, 9000.0, &displays()), None);
        assert_eq!(display_index_at(0.0, 0.0, &[]), None);
    }
}
