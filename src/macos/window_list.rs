use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGGetActiveDisplayList,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

pub(crate) const MAX_DISPLAYS: u32 = 32;

const LOCK_SCREEN_BUNDLE_ID: &str = "com.apple.loginwindow";

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[cfg(test)]
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn area(&self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    pub fn intersection_area(&self, other: &Rect) -> f64 {
        let left = self.x.max(other.x);
        let right = (self.x + self.width).min(other.x + other.width);
        let top = self.y.max(other.y);
        let bottom = (self.y + self.height).min(other.y + other.height);
        let width = right - left;
        let height = bottom - top;
        if width <= 0.0 || height <= 0.0 {
            return 0.0;
        }
        width * height
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowEntry {
    pub owner_pid: i32,
    pub owner_name: String,
    pub window_number: u32,
    pub bounds: Rect,
    pub layer: i32,
    pub z: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayEntry {
    pub index: usize,
    pub bounds: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningApplication {
    pub pid: i32,
    pub name: Option<String>,
    pub bundle_id: Option<String>,
}

pub fn window_list() -> Vec<WindowEntry> {
    windows_matching(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
    )
}

pub fn every_window() -> Vec<WindowEntry> {
    windows_matching(CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements)
}

fn windows_matching(options: CGWindowListOption) -> Vec<WindowEntry> {
    let Some(entries) = CGWindowListCopyWindowInfo(options, kCGNullWindowID) else {
        return Vec::new();
    };

    let entries = unsafe { entries.cast_unchecked::<CFType>() };
    let mut windows = Vec::with_capacity(entries.len());
    for z in 0..entries.len() {
        let Some(entry) = entries.get(z) else {
            continue;
        };
        let Some(entry) = entry.downcast_ref::<CFDictionary>() else {
            continue;
        };
        let Some(window) = window_entry(entry, z) else {
            continue;
        };
        windows.push(window);
    }
    windows
}

pub fn display_list() -> Vec<DisplayEntry> {
    let mut ids: [CGDirectDisplayID; MAX_DISPLAYS as usize] = [0; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;
    let status = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if status.0 != 0 {
        tracing::warn!(status = status.0, "could not read the active display list");
        return Vec::new();
    }

    let mut displays = Vec::with_capacity(count as usize);
    for (index, id) in ids.iter().take(count as usize).enumerate() {
        displays.push(DisplayEntry {
            index,
            bounds: rect_from_cg(CGDisplayBounds(*id)),
        });
    }
    displays
}

pub fn frontmost_application() -> Option<RunningApplication> {
    frontmost_of(
        super::ax::focused_application(),
        &window_list(),
        application_for_pid,
    )
}

fn frontmost_of<F>(
    focused_pid: Option<i32>,
    windows: &[WindowEntry],
    application_of: F,
) -> Option<RunningApplication>
where
    F: Fn(i32) -> Option<RunningApplication>,
{
    if let Some(application) = focused_of(focused_pid, &application_of) {
        return Some(application);
    }

    for window in windows {
        let WindowEntry {
            layer, owner_pid, ..
        } = window;
        if *layer != 0 {
            continue;
        }
        let Some(application) = application_of(*owner_pid) else {
            continue;
        };
        if !is_application(&application) {
            continue;
        }
        return Some(application);
    }
    None
}

fn focused_of<F>(focused_pid: Option<i32>, application_of: &F) -> Option<RunningApplication>
where
    F: Fn(i32) -> Option<RunningApplication>,
{
    let pid = focused_pid?;
    let Some(application) = application_of(pid) else {
        return Some(RunningApplication {
            pid,
            name: None,
            bundle_id: None,
        });
    };
    if is_lock_screen(&application) {
        return None;
    }
    Some(application)
}

pub fn is_lock_screen(application: &RunningApplication) -> bool {
    let RunningApplication { bundle_id, .. } = application;
    match bundle_id {
        Some(bundle_id) => bundle_id == LOCK_SCREEN_BUNDLE_ID,
        None => false,
    }
}

fn is_application(application: &RunningApplication) -> bool {
    let RunningApplication { bundle_id, .. } = application;
    match bundle_id {
        Some(bundle_id) => !bundle_id.is_empty(),
        None => false,
    }
}

pub(super) fn is_regular_application(pid: i32) -> bool {
    let Some(application) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
    else {
        return false;
    };
    application.activationPolicy() == NSApplicationActivationPolicy::Regular
}

pub(super) fn application_for_pid(pid: i32) -> Option<RunningApplication> {
    let application = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    Some(running_application(&application))
}

pub fn bundle_id_for_pid(pid: i32) -> Option<String> {
    let application = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let bundle_id = application.bundleIdentifier()?;
    Some(bundle_id.to_string())
}

pub(super) fn running_application(application: &NSRunningApplication) -> RunningApplication {
    let name = application.localizedName();
    let bundle_id = application.bundleIdentifier();
    RunningApplication {
        pid: application.processIdentifier(),
        name: name.map(|name| name.to_string()),
        bundle_id: bundle_id.map(|bundle_id| bundle_id.to_string()),
    }
}

fn window_entry(entry: &CFDictionary, z: usize) -> Option<WindowEntry> {
    let owner_pid = dictionary_i64(entry, unsafe { kCGWindowOwnerPID })?;
    let window_number = dictionary_i64(entry, unsafe { kCGWindowNumber })?;
    let layer = dictionary_i64(entry, unsafe { kCGWindowLayer })?;
    let bounds = dictionary_rect(entry, unsafe { kCGWindowBounds })?;
    let owner_name = dictionary_string(entry, unsafe { kCGWindowOwnerName }).unwrap_or_default();

    Some(WindowEntry {
        owner_pid: owner_pid as i32,
        owner_name,
        window_number: window_number as u32,
        bounds,
        layer: layer as i32,
        z,
    })
}

fn dictionary_value(dictionary: &CFDictionary, key: &CFString) -> Option<CFRetained<CFType>> {
    let dictionary = unsafe { dictionary.cast_unchecked::<CFType, CFType>() };
    dictionary.get(key.as_ref())
}

fn dictionary_string(dictionary: &CFDictionary, key: &CFString) -> Option<String> {
    let value = dictionary_value(dictionary, key)?;
    let value = value.downcast_ref::<CFString>()?;
    Some(value.to_string())
}

fn dictionary_i64(dictionary: &CFDictionary, key: &CFString) -> Option<i64> {
    let value = dictionary_value(dictionary, key)?;
    let value = value.downcast_ref::<CFNumber>()?;
    value.as_i64()
}

fn dictionary_rect(dictionary: &CFDictionary, key: &CFString) -> Option<Rect> {
    let value = dictionary_value(dictionary, key)?;
    let value = value.downcast_ref::<CFDictionary>()?;
    let mut rect = CGRect::default();
    let converted = unsafe { CGRectMakeWithDictionaryRepresentation(Some(value), &raw mut rect) };
    if !converted {
        return None;
    }
    Some(rect_from_cg(rect))
}

fn rect_from_cg(rect: CGRect) -> Rect {
    let CGRect { origin, size } = rect;
    Rect {
        x: origin.x,
        y: origin.y,
        width: size.width,
        height: size.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_of_a_normal_rectangle() {
        assert_eq!(Rect::new(0.0, 0.0, 100.0, 50.0).area(), 5000.0);
    }

    #[test]
    fn area_of_a_degenerate_rectangle_is_zero() {
        assert_eq!(Rect::new(0.0, 0.0, -100.0, 50.0).area(), 0.0);
        assert_eq!(Rect::new(0.0, 0.0, 100.0, 0.0).area(), 0.0);
    }

    #[test]
    fn disjoint_rectangles_do_not_intersect() {
        let window = Rect::new(0.0, 0.0, 100.0, 100.0);
        let display = Rect::new(200.0, 200.0, 100.0, 100.0);
        assert_eq!(window.intersection_area(&display), 0.0);
    }

    #[test]
    fn touching_edges_do_not_intersect() {
        let window = Rect::new(0.0, 0.0, 100.0, 100.0);
        let display = Rect::new(100.0, 0.0, 100.0, 100.0);
        assert_eq!(window.intersection_area(&display), 0.0);
    }

    #[test]
    fn contained_rectangle_intersects_entirely() {
        let window = Rect::new(10.0, 10.0, 80.0, 40.0);
        let display = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(window.intersection_area(&display), window.area());
    }

    #[test]
    fn partial_overlap_is_the_shared_area() {
        let window = Rect::new(-20.0, -10.0, 40.0, 30.0);
        let display = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(window.intersection_area(&display), 20.0 * 20.0);
    }

    #[test]
    fn negative_origin_displays_intersect_correctly() {
        let window = Rect::new(-5103.0, 1027.0, 2544.0, 1394.0);
        let display = Rect::new(-5120.0, 0.0, 2560.0, 1440.0);
        assert_eq!(window.intersection_area(&display), 2543.0 * 413.0);
    }

    #[test]
    fn a_single_pixel_overlap_is_one_unit_of_area() {
        let window = Rect::new(99.0, 99.0, 50.0, 50.0);
        let display = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(window.intersection_area(&display), 1.0);
    }

    #[test]
    fn contains_excludes_the_far_edges() {
        let display = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(display.contains(0.0, 0.0));
        assert!(display.contains(99.9, 99.9));
        assert!(!display.contains(100.0, 50.0));
        assert!(!display.contains(50.0, 100.0));
        assert!(!display.contains(-0.1, 50.0));
    }

    fn entry(owner_pid: i32, layer: i32, z: usize) -> WindowEntry {
        WindowEntry {
            owner_pid,
            owner_name: String::new(),
            window_number: z as u32,
            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
            layer,
            z,
        }
    }

    fn application(pid: i32, bundle_id: Option<&str>) -> RunningApplication {
        RunningApplication {
            pid,
            name: Some(format!("application {pid}")),
            bundle_id: bundle_id.map(|bundle_id| bundle_id.to_string()),
        }
    }

    fn applications(known: Vec<RunningApplication>) -> impl Fn(i32) -> Option<RunningApplication> {
        move |pid| {
            let mut found = None;
            for candidate in &known {
                if candidate.pid == pid {
                    found = Some(candidate.clone());
                    break;
                }
            }
            found
        }
    }

    fn every_owner_is_an_application(pid: i32) -> Option<RunningApplication> {
        Some(application(pid, Some("dev.pkarpovich.example")))
    }

    #[test]
    fn accessibility_names_the_focused_application_even_with_windows_in_front() {
        let windows = vec![entry(10, 0, 0), entry(20, 0, 1)];
        let front = frontmost_of(Some(20), &windows, every_owner_is_an_application);
        assert_eq!(front.map(|front| front.pid), Some(20));
    }

    #[test]
    fn a_pid_accessibility_names_but_nobody_can_name_is_reported_by_its_pid_alone() {
        let front = frontmost_of(Some(4321), &[], |_| None);
        assert_eq!(
            front,
            Some(RunningApplication {
                pid: 4321,
                name: None,
                bundle_id: None,
            })
        );
    }

    #[test]
    fn a_locked_screen_reports_what_is_still_on_screen_rather_than_the_lock_screen() {
        let windows = vec![entry(10, 0, 0)];
        let known = applications(vec![
            application(500, Some("com.apple.loginwindow")),
            application(10, Some("dev.pkarpovich.example")),
        ]);
        let front = frontmost_of(Some(500), &windows, known);
        assert_eq!(front.map(|front| front.pid), Some(10));
    }

    #[test]
    fn a_locked_screen_over_nothing_real_names_nobody() {
        let known = applications(vec![application(500, Some("com.apple.loginwindow"))]);
        assert!(frontmost_of(Some(500), &[], known).is_none());
    }

    #[test]
    fn a_real_application_from_accessibility_is_never_overridden_by_the_window_list() {
        let windows = vec![entry(10, 0, 0), entry(30, 0, 1)];
        let known = applications(vec![
            application(10, Some("dev.pkarpovich.other")),
            application(20, Some("dev.pkarpovich.example")),
        ]);
        let front = frontmost_of(Some(20), &windows, known);
        assert_eq!(front.map(|front| front.pid), Some(20));
    }

    #[test]
    fn the_topmost_ordinary_window_answers_when_accessibility_is_silent() {
        let windows = vec![entry(10, 0, 0), entry(20, 0, 1)];
        let front = frontmost_of(None, &windows, every_owner_is_an_application);
        assert_eq!(front.map(|front| front.pid), Some(10));
    }

    #[test]
    fn windows_above_the_ordinary_layer_are_skipped() {
        let windows = vec![entry(99, 25, 0), entry(10, 0, 1)];
        let front = frontmost_of(None, &windows, every_owner_is_an_application);
        assert_eq!(front.map(|front| front.pid), Some(10));
    }

    #[test]
    fn an_overlay_without_a_bundle_id_yields_to_the_next_real_application() {
        let windows = vec![entry(99, 0, 0), entry(10, 0, 1)];
        let known = applications(vec![
            application(99, Some("")),
            application(10, Some("dev.pkarpovich.example")),
        ]);
        let front = frontmost_of(None, &windows, known);
        assert_eq!(front.map(|front| front.pid), Some(10));
    }

    #[test]
    fn an_owner_no_running_application_names_is_an_overlay_too() {
        let windows = vec![entry(99, 0, 0), entry(10, 0, 1)];
        let known = applications(vec![
            application(99, None),
            application(10, Some("dev.pkarpovich.example")),
        ]);
        let front = frontmost_of(None, &windows, known);
        assert_eq!(front.map(|front| front.pid), Some(10));
    }

    #[test]
    fn a_list_of_nothing_but_overlays_names_nobody() {
        let windows = vec![entry(99, 0, 0), entry(98, 0, 1)];
        let known = applications(vec![application(99, Some("")), application(98, None)]);
        assert!(frontmost_of(None, &windows, known).is_none());
    }

    #[test]
    fn a_list_without_an_ordinary_window_names_nobody() {
        let windows = vec![entry(99, 25, 0), entry(98, -1, 1)];
        assert!(frontmost_of(None, &windows, every_owner_is_an_application).is_none());
    }

    #[test]
    fn an_empty_list_names_nobody() {
        assert!(frontmost_of(None, &[], every_owner_is_an_application).is_none());
    }
}
