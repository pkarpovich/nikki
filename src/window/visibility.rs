use crate::macos::window_list::{DisplayEntry, Rect, WindowEntry};

pub const VISIBLE_AREA_FRACTION: f64 = 0.2;

#[derive(Debug, Clone, PartialEq)]
pub struct VisibleWindow {
    pub window: WindowEntry,
    pub display: usize,
}

pub fn visible_windows(windows: &[WindowEntry], displays: &[DisplayEntry]) -> Vec<VisibleWindow> {
    let mut visible = Vec::new();
    for window in windows {
        if window.layer != 0 {
            continue;
        }
        let Some(display) = best_display(&window.bounds, displays) else {
            tracing::debug!(
                owner = %window.owner_name,
                window_number = window.window_number,
                z = window.z,
                "window intersects no active display and is not visible"
            );
            continue;
        };
        visible.push(VisibleWindow {
            window: window.clone(),
            display,
        });
    }
    visible
}

fn best_display(bounds: &Rect, displays: &[DisplayEntry]) -> Option<usize> {
    let area = bounds.area();
    if area <= 0.0 {
        return None;
    }
    let threshold = area * VISIBLE_AREA_FRACTION;

    let mut best: Option<(usize, f64)> = None;
    for DisplayEntry {
        index,
        bounds: display_bounds,
    } in displays
    {
        let overlap = bounds.intersection_area(display_bounds);
        if overlap < threshold {
            continue;
        }
        let Some((best_index, best_overlap)) = best else {
            best = Some((*index, overlap));
            continue;
        };
        if overlap > best_overlap || (overlap == best_overlap && *index < best_index) {
            best = Some((*index, overlap));
        }
    }

    let (index, _) = best?;
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::window_list::Rect;

    fn display(index: usize, bounds: Rect) -> DisplayEntry {
        DisplayEntry { index, bounds }
    }

    fn window(z: usize, bounds: Rect) -> WindowEntry {
        WindowEntry {
            owner_pid: 501 + z as i32,
            owner_name: format!("App{z}"),
            window_number: 900 + z as u32,
            bounds,
            layer: 0,
            z,
        }
    }

    #[test]
    fn a_window_wholly_inside_one_display_is_visible_on_it() {
        let displays = vec![
            display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0)),
            display(1, Rect::new(1000.0, 0.0, 1000.0, 1000.0)),
        ];
        let windows = vec![window(0, Rect::new(1100.0, 100.0, 400.0, 300.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 1);
        assert_eq!(visible[0].window, windows[0]);
    }

    #[test]
    fn a_window_wholly_outside_every_display_is_not_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let windows = vec![window(0, Rect::new(5000.0, 5000.0, 400.0, 300.0))];

        assert!(visible_windows(&windows, &displays).is_empty());
    }

    #[test]
    fn a_straddling_window_lands_on_the_display_with_the_larger_overlap() {
        let displays = vec![
            display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0)),
            display(1, Rect::new(1000.0, 0.0, 1000.0, 1000.0)),
        ];
        let windows = vec![window(0, Rect::new(700.0, 0.0, 400.0, 100.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 0);
    }

    #[test]
    fn an_exact_tie_goes_to_the_lower_display_index() {
        let displays = vec![
            display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0)),
            display(1, Rect::new(1000.0, 0.0, 1000.0, 1000.0)),
        ];
        let windows = vec![window(0, Rect::new(800.0, 0.0, 400.0, 100.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 0);
    }

    #[test]
    fn an_exact_tie_follows_the_index_rather_than_the_list_order() {
        let displays = vec![
            display(1, Rect::new(1000.0, 0.0, 1000.0, 1000.0)),
            display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0)),
        ];
        let windows = vec![window(0, Rect::new(800.0, 0.0, 400.0, 100.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 0);
    }

    #[test]
    fn a_single_pixel_of_overlap_is_not_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let windows = vec![window(0, Rect::new(999.0, 999.0, 100.0, 100.0))];

        assert!(visible_windows(&windows, &displays).is_empty());
    }

    #[test]
    fn exactly_the_threshold_is_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 20.0, 100.0))];
        let windows = vec![window(0, Rect::new(0.0, 0.0, 100.0, 100.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 0);
    }

    #[test]
    fn just_below_the_threshold_is_not_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 19.0, 100.0))];
        let windows = vec![window(0, Rect::new(0.0, 0.0, 100.0, 100.0))];

        assert!(visible_windows(&windows, &displays).is_empty());
    }

    #[test]
    fn a_non_zero_layer_window_is_never_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let mut chrome = window(0, Rect::new(0.0, 0.0, 400.0, 300.0));
        chrome.layer = 25;
        let windows = vec![chrome];

        assert!(visible_windows(&windows, &displays).is_empty());
    }

    #[test]
    fn a_window_on_a_display_absent_from_the_list_is_not_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let windows = vec![
            window(0, Rect::new(100.0, 100.0, 400.0, 300.0)),
            window(1, Rect::new(-2560.0, 0.0, 400.0, 300.0)),
        ];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].window.z, 0);
    }

    #[test]
    fn an_empty_display_list_makes_every_window_invisible() {
        let windows = vec![window(0, Rect::new(100.0, 100.0, 400.0, 300.0))];

        assert!(visible_windows(&windows, &[]).is_empty());
    }

    #[test]
    fn a_degenerate_window_is_not_visible() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let windows = vec![window(0, Rect::new(100.0, 100.0, 0.0, 300.0))];

        assert!(visible_windows(&windows, &displays).is_empty());
    }

    #[test]
    fn front_to_back_order_is_preserved() {
        let displays = vec![display(0, Rect::new(0.0, 0.0, 1000.0, 1000.0))];
        let windows = vec![
            window(0, Rect::new(0.0, 0.0, 400.0, 300.0)),
            window(1, Rect::new(9000.0, 0.0, 400.0, 300.0)),
            window(2, Rect::new(200.0, 200.0, 400.0, 300.0)),
        ];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].window.z, 0);
        assert_eq!(visible[1].window.z, 2);
    }

    #[test]
    fn a_window_spanning_three_displays_takes_the_largest_share() {
        let displays = vec![
            display(0, Rect::new(0.0, 0.0, 100.0, 100.0)),
            display(1, Rect::new(100.0, 0.0, 100.0, 100.0)),
            display(2, Rect::new(200.0, 0.0, 100.0, 100.0)),
        ];
        let windows = vec![window(0, Rect::new(50.0, 0.0, 200.0, 100.0))];

        let visible = visible_windows(&windows, &displays);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].display, 1);
    }
}
