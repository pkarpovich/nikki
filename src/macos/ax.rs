use std::ptr::NonNull;

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType, Type};

pub(super) const MESSAGING_TIMEOUT_SECONDS: f32 = 0.4;

pub(super) const ATTRIBUTE_WINDOWS: &str = "AXWindows";
pub(super) const ATTRIBUTE_TITLE: &str = "AXTitle";
pub(super) const ATTRIBUTE_FOCUSED_WINDOW: &str = "AXFocusedWindow";
pub(super) const ATTRIBUTE_DOCUMENT: &str = "AXDocument";
pub(super) const ATTRIBUTE_FOCUSED_APPLICATION: &str = "AXFocusedApplication";

pub fn accessibility_is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

#[allow(dead_code, reason = "becomes the focus source in task 2")]
pub fn focused_application() -> Option<i32> {
    let system_wide = unsafe { AXUIElement::new_system_wide() };
    apply_messaging_timeout(&system_wide);
    focused_application_of(&system_wide)
}

pub struct AxApplication {
    element: CFRetained<AXUIElement>,
}

impl AxApplication {
    pub fn for_pid(pid: i32) -> Self {
        let element = unsafe { AXUIElement::new_application(pid) };
        apply_messaging_timeout(&element);
        Self { element }
    }

    pub fn windows(&self) -> Vec<AxWindow> {
        let elements = attribute_elements(&self.element, ATTRIBUTE_WINDOWS);
        let mut windows = Vec::with_capacity(elements.len());
        for element in elements {
            windows.push(AxWindow { element });
        }
        windows
    }

    #[cfg(test)]
    pub fn window_count(&self) -> usize {
        attribute_elements(&self.element, ATTRIBUTE_WINDOWS).len()
    }

    pub fn focused_window(&self) -> Option<AxWindow> {
        let element = attribute_element(&self.element, ATTRIBUTE_FOCUSED_WINDOW)?;
        Some(AxWindow { element })
    }

    #[cfg(test)]
    pub(super) fn element(&self) -> &AXUIElement {
        &self.element
    }
}

pub struct AxWindow {
    element: CFRetained<AXUIElement>,
}

impl AxWindow {
    pub fn title(&self) -> Option<String> {
        attribute_string(&self.element, ATTRIBUTE_TITLE)
    }

    pub fn document(&self) -> Option<String> {
        attribute_string(&self.element, ATTRIBUTE_DOCUMENT)
    }
}

fn apply_messaging_timeout(element: &AXUIElement) {
    let status = unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECONDS) };
    if status == AXError::Success {
        return;
    }
    tracing::debug!(status = status.0, "could not set the accessibility timeout");
}

fn focused_application_of(system_wide: &AXUIElement) -> Option<i32> {
    let element = attribute_element(system_wide, ATTRIBUTE_FOCUSED_APPLICATION)?;
    element_pid(&element)
}

fn element_pid(element: &AXUIElement) -> Option<i32> {
    let mut pid: libc::pid_t = 0;
    let status = unsafe { element.pid(NonNull::from(&mut pid)) };
    pid_of(status, pid)
}

fn pid_of(status: AXError, pid: libc::pid_t) -> Option<i32> {
    if status != AXError::Success {
        return None;
    }
    if pid <= 0 {
        return None;
    }
    Some(pid)
}

fn copy_attribute(element: &AXUIElement, attribute: &str) -> Option<CFRetained<CFType>> {
    let attribute = CFString::from_str(attribute);
    let mut value: *const CFType = std::ptr::null();
    let status = unsafe { element.copy_attribute_value(&attribute, NonNull::from(&mut value)) };
    if status != AXError::Success {
        return None;
    }
    let value = NonNull::new(value.cast_mut())?;
    Some(unsafe { CFRetained::from_raw(value) })
}

fn attribute_string(element: &AXUIElement, attribute: &str) -> Option<String> {
    let value = copy_attribute(element, attribute)?;
    let Some(value) = value.downcast_ref::<CFString>() else {
        tracing::debug!(attribute, "accessibility attribute was not a string");
        return None;
    };
    Some(value.to_string())
}

fn attribute_element(element: &AXUIElement, attribute: &str) -> Option<CFRetained<AXUIElement>> {
    let value = copy_attribute(element, attribute)?;
    let Some(value) = value.downcast_ref::<AXUIElement>() else {
        tracing::debug!(attribute, "accessibility attribute was not an element");
        return None;
    };
    let value = value.retain();
    apply_messaging_timeout(&value);
    Some(value)
}

fn attribute_elements(element: &AXUIElement, attribute: &str) -> Vec<CFRetained<AXUIElement>> {
    let Some(value) = copy_attribute(element, attribute) else {
        return Vec::new();
    };
    let Some(value) = value.downcast_ref::<CFArray>() else {
        tracing::debug!(attribute, "accessibility attribute was not an array");
        return Vec::new();
    };
    let value = unsafe { value.cast_unchecked::<CFType>() };

    let mut elements = Vec::with_capacity(value.len());
    for index in 0..value.len() {
        let Some(entry) = value.get(index) else {
            continue;
        };
        let Some(entry) = entry.downcast_ref::<AXUIElement>() else {
            continue;
        };
        let entry = entry.retain();
        apply_messaging_timeout(&entry);
        elements.push(entry);
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_names_match_the_accessibility_constants() {
        assert_eq!(ATTRIBUTE_WINDOWS, "AXWindows");
        assert_eq!(ATTRIBUTE_TITLE, "AXTitle");
        assert_eq!(ATTRIBUTE_FOCUSED_WINDOW, "AXFocusedWindow");
        assert_eq!(ATTRIBUTE_DOCUMENT, "AXDocument");
        assert_eq!(ATTRIBUTE_FOCUSED_APPLICATION, "AXFocusedApplication");
    }

    #[test]
    fn an_accessibility_error_is_no_answer_about_the_focused_application() {
        assert!(pid_of(AXError::CannotComplete, 4321).is_none());
        assert!(pid_of(AXError::APIDisabled, 4321).is_none());
        assert!(pid_of(AXError::InvalidUIElement, 0).is_none());
    }

    #[test]
    fn an_element_belonging_to_no_process_is_no_answer_about_the_focused_application() {
        assert!(pid_of(AXError::Success, 0).is_none());
        assert!(pid_of(AXError::Success, -1).is_none());
    }

    #[test]
    fn a_process_that_answered_is_the_focused_application() {
        assert_eq!(pid_of(AXError::Success, 4321), Some(4321));
    }

    #[test]
    fn an_element_that_names_no_focused_application_is_no_answer() {
        let application = AxApplication::for_pid(0);
        assert!(focused_application_of(application.element()).is_none());
        assert!(element_pid(application.element()).is_none());
    }

    #[test]
    #[ignore = "reads the live machine: needs an accessibility grant and an application in front"]
    fn the_live_machine_names_a_focused_application() {
        assert!(
            accessibility_is_trusted(),
            "accessibility is not granted, so no focus can be read"
        );
        let Some(front) = crate::macos::window_list::frontmost_application() else {
            panic!("no application is in front, so no focus can be read");
        };
        AxApplication::for_pid(front.pid).focused_window();

        let pid = focused_application();
        assert!(
            pid.is_some_and(|pid| pid > 0),
            "accessibility named no focused application"
        );
    }

    #[test]
    fn a_dead_process_yields_no_windows_and_no_focused_window() {
        let application = AxApplication::for_pid(0);
        assert_eq!(application.window_count(), 0);
        assert!(application.focused_window().is_none());
        assert!(application.windows().is_empty());
    }

    #[test]
    fn a_string_attribute_read_from_a_dead_process_is_absent() {
        let application = AxApplication::for_pid(0);
        assert!(attribute_string(application.element(), ATTRIBUTE_TITLE).is_none());
    }

    #[test]
    fn an_element_attribute_of_the_wrong_type_is_rejected() {
        let application = AxApplication::for_pid(0);
        assert!(attribute_element(application.element(), ATTRIBUTE_TITLE).is_none());
    }
}
