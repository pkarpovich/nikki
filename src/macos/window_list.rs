use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGGetActiveDisplayList,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

const MAX_DISPLAYS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
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
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
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
    let workspace = NSWorkspace::sharedWorkspace();
    let application = workspace.frontmostApplication()?;
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
}
