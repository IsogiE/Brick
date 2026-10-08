//! Native startup placement. Fit once before first show, never during repaint.
pub const INITIAL_INNER_SIZE: [f32; 2] = [1440.0, 980.0];
pub const MIN_INNER_SIZE: [f32; 2] = [980.0, 720.0];

#[cfg(any(target_os = "windows", test))]
mod layout {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    impl Rect {
        fn size(self) -> Option<[u32; 2]> {
            let width = i64::from(self.right) - i64::from(self.left);
            let height = i64::from(self.bottom) - i64::from(self.top);
            (width > 0 && height > 0)
                .then_some([u32::try_from(width).ok()?, u32::try_from(height).ok()?])
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Placement {
        pub position: [i32; 2],
        pub inner_size: [u32; 2],
        pub minimum_inner_size: [u32; 2],
    }

    pub(super) fn fit(
        outer: Rect,
        inner: [u32; 2],
        work: Rect,
        minimum_points: [f32; 2],
        pixels_per_point: f64,
    ) -> Option<Placement> {
        if !pixels_per_point.is_finite()
            || pixels_per_point <= 0.0
            || minimum_points.iter().any(|v| !v.is_finite() || *v <= 0.0)
            || inner.contains(&0)
        {
            return None;
        }
        let outer_size = outer.size()?;
        let work_size = work.size()?;
        let mut minimum = [0; 2];
        let mut size = [0; 2];
        let mut total = [0; 2];
        for axis in 0..2 {
            let decoration = outer_size[axis].checked_sub(inner[axis])?;
            let available = work_size[axis].checked_sub(decoration)?;
            if available == 0 {
                return None;
            }
            // The app's usual minimum cannot exceed the usable native client.
            minimum[axis] = ((f64::from(minimum_points[axis]) * pixels_per_point).round() as u32)
                .clamp(1, available);
            size[axis] = inner[axis].clamp(minimum[axis], available);
            total[axis] = size[axis].checked_add(decoration)?;
        }
        let x = i64::from(outer.left).clamp(
            i64::from(work.left),
            i64::from(work.right) - i64::from(total[0]),
        );
        let y = i64::from(outer.top).clamp(
            i64::from(work.top),
            i64::from(work.bottom) - i64::from(total[1]),
        );
        Some(Placement {
            position: [i32::try_from(x).ok()?, i32::try_from(y).ok()?],
            inner_size: size,
            minimum_inner_size: minimum,
        })
    }
}

/// Fit the already-created normal window to its actual monitor before first show.
///
/// Eframe's initial clamp uses full monitor dimensions for the client alone.
/// Windows decorations and the taskbar also need room. Measuring the HWND after
/// creation preserves its DPI and any valid restored placement. Maximized and
/// fullscreen placement remains under Windows control.
#[cfg(target_os = "windows")]
pub fn fit_initial_window(cc: &eframe::CreationContext<'_>) -> std::io::Result<()> {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use windows_sys::Win32::{
        Foundation::HWND,
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
    };

    let window = cc
        .winit_window()
        .ok_or_else(|| std::io::Error::other("Native window is unavailable"))?;
    if window.is_maximized() || window.fullscreen().is_some() {
        return Ok(());
    }
    let handle = cc
        .window_handle()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(std::io::Error::other("Expected a Windows window"));
    };
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // Eframe invokes AppCreator on the window's DPI-aware event-loop thread,
    // with a live hidden HWND. rcWork and winit's physical geometry therefore
    // share virtual-screen pixel coordinates, including negative monitor origins.
    unsafe {
        let monitor = MonitorFromWindow(handle.hwnd.get() as HWND, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut monitor_info) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let position = window
        .outer_position()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let outer_size = window.outer_size();
    let mut inner_size = window.inner_size();
    let right = i64::from(position.x) + i64::from(outer_size.width);
    let bottom = i64::from(position.y) + i64::from(outer_size.height);
    let outer = layout::Rect {
        left: position.x,
        top: position.y,
        right: i32::try_from(right).map_err(std::io::Error::other)?,
        bottom: i32::try_from(bottom).map_err(std::io::Error::other)?,
    };
    let work = monitor_info.rcWork;
    let placement = layout::fit(
        outer,
        [inner_size.width, inner_size.height],
        layout::Rect {
            left: work.left,
            top: work.top,
            right: work.right,
            bottom: work.bottom,
        },
        MIN_INNER_SIZE,
        window.scale_factor() * f64::from(cc.egui_ctx.zoom_factor()),
    )
    .ok_or_else(|| std::io::Error::other("Invalid monitor or window geometry"))?;

    let mut minimum = inner_size;
    minimum.width = placement.minimum_inner_size[0];
    minimum.height = placement.minimum_inner_size[1];
    // Update winit's cached minimum first: a resize must not be expanded beyond
    // a small work area. Store logical units so future DPI changes still scale it.
    window.set_min_inner_size(Some(minimum.to_logical::<f64>(window.scale_factor())));
    inner_size.width = placement.inner_size[0];
    inner_size.height = placement.inner_size[1];
    if inner_size != window.inner_size() {
        let _ = window.request_inner_size(inner_size);
    }
    let mut fitted_position = position;
    fitted_position.x = placement.position[0];
    fitted_position.y = placement.position[1];
    if fitted_position != position {
        window.set_outer_position(fitted_position);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        layout::{fit, Rect},
        MIN_INNER_SIZE,
    };

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        }
    }

    #[test]
    fn small_desktop_reserves_taskbar_and_decorations_and_relaxes_minimum() {
        let fitted = fit(
            rect(130, 130, 1296, 839),
            [1280, 800],
            rect(0, 0, 1280, 752),
            MIN_INNER_SIZE,
            1.0,
        )
        .unwrap();
        assert_eq!(fitted.position, [0, 0]);
        assert_eq!(fitted.inner_size, [1264, 713]);
        assert_eq!(fitted.minimum_inner_size, [980, 713]);
    }

    #[test]
    fn valid_saved_placement_is_preserved() {
        let fitted = fit(
            rect(150, 120, 1216, 839),
            [1200, 800],
            rect(0, 0, 1920, 1032),
            MIN_INNER_SIZE,
            1.0,
        )
        .unwrap();
        assert_eq!(fitted.position, [150, 120]);
        assert_eq!(fitted.inner_size, [1200, 800]);
        assert_eq!(fitted.minimum_inner_size, [980, 720]);
    }

    #[test]
    fn offscreen_saved_placement_moves_onto_selected_negative_origin_monitor() {
        let fitted = fit(
            rect(-2400, -800, 1216, 839),
            [1200, 800],
            rect(-1920, -200, 1920, 1032),
            MIN_INNER_SIZE,
            1.0,
        )
        .unwrap();
        assert_eq!(fitted.position, [-1920, -200]);
        assert_eq!(fitted.inner_size, [1200, 800]);
    }

    #[test]
    fn high_dpi_minimum_and_frame_are_measured_in_physical_pixels() {
        let fitted = fit(
            rect(100, 100, 1952, 1678),
            [1920, 1600],
            rect(0, 0, 1920, 1000),
            MIN_INNER_SIZE,
            2.0,
        )
        .unwrap();
        assert_eq!(fitted.position, [0, 0]);
        assert_eq!(fitted.inner_size, [1888, 922]);
        assert_eq!(fitted.minimum_inner_size, [1888, 922]);
    }

    #[test]
    fn top_and_left_taskbars_keep_the_window_in_the_work_area() {
        let fitted = fit(
            rect(0, 0, 1296, 839),
            [1280, 800],
            rect(48, 48, 1232, 752),
            MIN_INNER_SIZE,
            1.0,
        )
        .unwrap();
        assert_eq!(fitted.position, [48, 48]);
        assert_eq!(fitted.inner_size, [1216, 713]);
    }

    #[test]
    fn invalid_geometry_is_rejected_without_a_placement() {
        let work = rect(0, 0, 1280, 752);
        assert!(fit(rect(0, 0, 16, 39), [0, 0], work, MIN_INNER_SIZE, 1.0).is_none());
        assert!(fit(
            rect(0, 0, 1200, 800),
            [1280, 800],
            work,
            MIN_INNER_SIZE,
            1.0
        )
        .is_none());
        assert!(fit(
            rect(0, 0, 1296, 839),
            [1280, 800],
            work,
            MIN_INNER_SIZE,
            f64::NAN
        )
        .is_none());
        assert!(fit(
            rect(0, 0, 1296, 839),
            [1280, 800],
            rect(0, 0, 8, 8),
            MIN_INNER_SIZE,
            1.0
        )
        .is_none());
    }
}
