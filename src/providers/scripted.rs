use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::windows::{Activity, FocusedWindow, Sources, WindowTitle};
use crate::extract::Details;
use crate::macos::activity::InputCounters;
use crate::macos::window_list::{DisplayEntry, Rect, RunningApplication, WindowEntry};

/// The variable naming a scene file, which replaces what the daemon reads off the screen.
pub const TEST_SOURCES_VAR: &str = "NIKKI_TEST_SOURCES";

/// A scene: what a scripted run is told is on screen, in place of the machine's own answer.
///
/// It exists because the acceptance suite otherwise samples whatever the machine happens to be
/// showing, and a host with no window session - a CI runner between jobs - has no frontmost
/// application at all, so the daemon assembles nothing and every assertion about a window record
/// waits for something that can never arrive.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Scene {
    frontmost: Option<Application>,
    displays: Vec<Display>,
    windows: Vec<Window>,
    focused: Focused,
    activity: Ambient,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Application {
    pid: i32,
    name: Option<String>,
    bundle_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Display {
    index: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Window {
    pid: i32,
    name: String,
    number: u32,
    title: Option<String>,
    #[serde(default)]
    layer: i32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum Focused {
    Window {
        title: Option<String>,
        path: Option<String>,
    },
    #[default]
    Absent,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Ambient {
    idle_sec: i64,
    keys: u32,
    mouse: u32,
    mic_active: bool,
    screen_locked: bool,
    display_asleep: bool,
}

/// Answers every question about the screen from a scene file rather than from the machine.
#[derive(Debug)]
pub struct ScriptedSources {
    scene: Scene,
}

impl ScriptedSources {
    /// Reads the scene, refusing a file it cannot parse rather than sampling something else.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) => return Err(format!("{} could not be read: {source}", path.display())),
        };
        let scene = match serde_json::from_str(&text) {
            Ok(scene) => scene,
            Err(source) => return Err(format!("{} is not a scene: {source}", path.display())),
        };
        Ok(Self { scene })
    }
}

impl Sources for ScriptedSources {
    fn windows(&self) -> Vec<WindowEntry> {
        let mut entries = Vec::with_capacity(self.scene.windows.len());
        for (z, window) in self.scene.windows.iter().enumerate() {
            let Window {
                pid,
                name,
                number,
                title: _,
                layer,
                x,
                y,
                width,
                height,
            } = window;
            entries.push(WindowEntry {
                owner_pid: *pid,
                owner_name: name.clone(),
                window_number: *number,
                bounds: Rect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                },
                layer: *layer,
                z,
            });
        }
        entries
    }

    fn displays(&self) -> Vec<DisplayEntry> {
        let mut entries = Vec::with_capacity(self.scene.displays.len());
        for display in &self.scene.displays {
            let Display {
                index,
                x,
                y,
                width,
                height,
            } = display;
            entries.push(DisplayEntry {
                index: *index,
                bounds: Rect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                },
            });
        }
        entries
    }

    fn frontmost(&self) -> Option<RunningApplication> {
        let Application {
            pid,
            name,
            bundle_id,
        } = self.scene.frontmost.as_ref()?;
        Some(RunningApplication {
            pid: *pid,
            name: name.clone(),
            bundle_id: bundle_id.clone(),
        })
    }

    fn bundle_id(&self, pid: i32) -> Option<String> {
        let Application {
            pid: known,
            bundle_id,
            ..
        } = self.scene.frontmost.as_ref()?;
        match *known == pid {
            true => bundle_id.clone(),
            false => None,
        }
    }

    fn cursor_display(&self, displays: &[DisplayEntry]) -> Option<usize> {
        let DisplayEntry { index, .. } = displays.first()?;
        Some(*index)
    }

    fn focused_window(&self, _pid: i32) -> FocusedWindow {
        match &self.scene.focused {
            Focused::Window { title, path } => FocusedWindow::Window {
                title: title.clone(),
                path: path.clone(),
            },
            Focused::Absent => FocusedWindow::Absent,
            Focused::Unavailable => FocusedWindow::Unavailable,
        }
    }

    fn window_title(&self, pid: i32) -> WindowTitle {
        let mut found = None;
        let mut count = 0;
        for window in &self.scene.windows {
            if window.pid != pid {
                continue;
            }
            count += 1;
            found = Some(window.title.clone());
        }
        match count {
            1 => WindowTitle::Sole(found.unwrap_or_default()),
            _ => WindowTitle::Ambiguous,
        }
    }

    fn activity(&self) -> Activity {
        let Ambient {
            idle_sec,
            keys,
            mouse,
            mic_active,
            screen_locked,
            display_asleep,
        } = self.scene.activity;
        Activity {
            idle_sec,
            counters: InputCounters { keys, mouse },
            mic_active,
            screen_locked,
            display_asleep,
        }
    }

    fn rescan_observers(&self) {}

    async fn details(&self, _bundle_id: &str) -> Details {
        Details::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

    fn scene_file(body: &str) -> std::path::PathBuf {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("nikki-scene-test-{}-{id}.json", std::process::id()));
        fs::write(&path, body).expect("write the scene");
        path
    }

    fn loaded(body: &str) -> ScriptedSources {
        let path = scene_file(body);
        let sources = ScriptedSources::load(&path).expect("the scene loads");
        let _ = fs::remove_file(&path);
        sources
    }

    #[test]
    fn a_scene_answers_what_is_on_screen() {
        let sources = loaded(
            r#"{
                "frontmost": {"pid": 42, "name": "Dia", "bundle_id": "company.thebrowser.dia"},
                "displays": [{"index": 0, "x": 0, "y": 0, "width": 1440, "height": 900}],
                "windows": [
                    {"pid": 42, "name": "Dia", "number": 7, "title": "a tab",
                     "x": 0, "y": 0, "width": 1440, "height": 900}
                ],
                "focused": {"window": {"title": "a tab", "path": null}},
                "activity": {"idle_sec": 3, "keys": 5, "mouse": 2}
            }"#,
        );

        let RunningApplication {
            pid,
            name,
            bundle_id,
        } = sources
            .frontmost()
            .expect("the scene names a frontmost application");
        assert_eq!(pid, 42);
        assert_eq!(name.as_deref(), Some("Dia"));
        assert_eq!(bundle_id.as_deref(), Some("company.thebrowser.dia"));
        assert_eq!(
            sources.bundle_id(42).as_deref(),
            Some("company.thebrowser.dia")
        );
        assert_eq!(sources.bundle_id(43), None);

        let windows = sources.windows();
        assert_eq!(windows.len(), 1);
        let WindowEntry {
            owner_pid,
            window_number,
            layer,
            z,
            ..
        } = windows[0];
        assert_eq!((owner_pid, window_number, layer, z), (42, 7, 0, 0));

        assert_eq!(sources.cursor_display(&sources.displays()), Some(0));
        assert_eq!(
            sources.focused_window(42),
            FocusedWindow::Window {
                title: Some("a tab".to_string()),
                path: None,
            }
        );
        assert_eq!(
            sources.window_title(42),
            WindowTitle::Sole(Some("a tab".to_string()))
        );

        let Activity {
            idle_sec, counters, ..
        } = sources.activity();
        assert_eq!(idle_sec, 3);
        assert_eq!(counters, InputCounters { keys: 5, mouse: 2 });
    }

    #[test]
    fn an_empty_scene_is_a_machine_showing_nothing() {
        let sources = loaded("{}");

        assert_eq!(sources.frontmost(), None);
        assert!(sources.windows().is_empty());
        assert!(sources.displays().is_empty());
        assert_eq!(sources.focused_window(1), FocusedWindow::Absent);
        assert_eq!(sources.window_title(1), WindowTitle::Ambiguous);
        assert_eq!(sources.activity(), Activity::default());
    }

    #[test]
    fn several_windows_of_one_application_leave_the_title_ambiguous() {
        let sources = loaded(
            r#"{
                "windows": [
                    {"pid": 9, "name": "Ghostty", "number": 1, "x": 0, "y": 0, "width": 10, "height": 10},
                    {"pid": 9, "name": "Ghostty", "number": 2, "x": 0, "y": 0, "width": 10, "height": 10}
                ]
            }"#,
        );

        assert_eq!(sources.window_title(9), WindowTitle::Ambiguous);
        assert_eq!(
            sources.windows()[1].z,
            1,
            "z follows the order of the scene"
        );
    }

    #[test]
    fn a_scene_that_is_not_json_is_refused_rather_than_ignored() {
        let path = scene_file("{ not json");
        let error = ScriptedSources::load(&path).expect_err("a malformed scene is an error");
        let _ = fs::remove_file(&path);
        assert!(error.contains("is not a scene"), "{error}");
    }

    #[test]
    fn a_scene_with_an_unknown_field_is_refused() {
        let path = scene_file(r#"{"frontmsot": {"pid": 1}}"#);
        let error = ScriptedSources::load(&path).expect_err("a typo is an error");
        let _ = fs::remove_file(&path);
        assert!(error.contains("is not a scene"), "{error}");
    }

    #[test]
    fn a_missing_scene_names_the_file() {
        let missing = std::env::temp_dir().join("nikki-scene-test-absent.json");
        let error = ScriptedSources::load(&missing).expect_err("a missing scene is an error");
        assert!(error.contains("could not be read"), "{error}");
    }
}
