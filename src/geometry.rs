//! Pure screen geometry: the usable desktop rectangle, the notification
//! area, and the strip's placement inside them. No Win32 types — fully
//! unit-tested.

/// Rectangle in physical screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Rect {
        Rect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }

    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Place the strip inside the bottom taskbar band; horizontally anchored
/// per `right_align`.
///
/// `work` is the monitor's work area — the desktop minus whatever the
/// taskbar reserves, whichever edge the bar is docked to. `tray` is the
/// shell's notification-area container (`TrayNotifyWnd`). When the bar is
/// bottom-docked (the work area stops above it) and the tray lives in that
/// band, the strip is drawn **inside the band**, vertically centred. Any
/// other layout (top/side-docked or auto-hidden bar) keeps the strip
/// floating in the desktop's bottom corner, where the band either does not
/// exist or is not visible.
///
/// Horizontal anchor:
/// - `right_align = false`: the strip starts at the area's left margin
///   (bottom-left, v1 parity).
/// - `right_align = true`: the strip ends at the notification area's left
///   edge and never overlaps the chevron, tray icons or clock. An in-band
///   obstacle (the TrafficMonitor taskbar window) that would collide with
///   the strip shifts the limit to the obstacle's left when the strip can
///   fit entirely there.
pub fn place(
    work: &Rect,
    monitor: &Rect,
    tray: Option<Rect>,
    obstacle: Option<Rect>,
    right_align: bool,
    win_w: i32,
    win_h: i32,
    margin: i32,
) -> Rect {
    let area = if work.width() > 0 && work.height() > 0 {
        work
    } else {
        // No work area reported: fall back to the whole screen rather
        // than drawing the strip at a negative coordinate.
        monitor
    };
    // Bottom band: the slot the bar reserves between the work area and the
    // bottom edge of the monitor.
    let band = (area.bottom < monitor.bottom).then_some(Rect {
        left: monitor.left,
        top: area.bottom,
        right: monitor.right,
        bottom: monitor.bottom,
    });
    let tray_in_band = match tray {
        Some(t) if t.top + 1 >= area.bottom && t.left > area.left => Some(t),
        _ => None,
    };
    let x = if right_align {
        // Right edge at the tray (or an in-band obstacle), clamped so the
        // strip never goes past the left margin.
        let mut x_limit = tray_in_band.map_or(area.right, |t| t.left);
        // Step over an in-band obstacle (the TrafficMonitor taskbar
        // window) when the strip would land on it and can fit entirely
        // to its left.
        if let Some(ob) = obstacle.filter(|o| o.top + 1 >= area.bottom && o.left > area.left) {
            let overlaps = x_limit - margin - win_w < ob.right && x_limit > ob.left;
            if overlaps && ob.left - margin - win_w >= area.left + margin {
                x_limit = ob.left;
            }
        }
        (x_limit - margin - win_w).max(area.left + margin)
    } else {
        // Left edge anchor: the strip always starts at the desktop's left
        // margin, regardless of where the tray or any obstacle sits.
        area.left + margin
    };
    let y = match (band, tray_in_band) {
        (Some(b), Some(_)) if b.height() >= win_h => b.top + (b.height() - win_h) / 2,
        (Some(b), Some(_)) => b.top, // band shorter than the strip: top-align
        _ => area.bottom - margin - win_h,
    };
    Rect::new(x, y, win_w, win_h)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MON: Rect = Rect {
        left: 0,
        top: 0,
        right: 3840,
        bottom: 1600,
    };
    const MARGIN: i32 = 10;

    /// Bottom-docked bar (48 px): the work area ends above it, and the
    /// notification area (chevron, tray icons, clock) starts at 3526.
    const BOTTOM_WORK: Rect = Rect {
        left: 0,
        top: 0,
        right: 3840,
        bottom: 1552,
    };
    const BOTTOM_TRAY: Rect = Rect {
        left: 3526,
        top: 1552,
        right: 3840,
        bottom: 1600,
    };

    #[test]
    fn strip_is_drawn_inside_the_band_at_the_left_edge() {
        let r = place(&BOTTOM_WORK, &MON, Some(BOTTOM_TRAY), None, false, 500, 45, MARGIN);
        // Band 1552..1600 is 48 px tall, the strip is 45: centred at 1553.
        assert_eq!(r, Rect::new(MARGIN, 1553, 500, 45));
        assert!(r.top >= BOTTOM_WORK.bottom, "inside the bar band");
        assert!(r.bottom <= MON.bottom);
        assert_eq!(r.left, MARGIN, "anchored to the left edge");
    }

    #[test]
    fn missing_notification_area_floats_above_the_bar() {
        let r = place(&BOTTOM_WORK, &MON, None, None, false, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(MARGIN, 1497, 500, 45));
        assert!(r.bottom <= BOTTOM_WORK.bottom, "clear of the bar");
    }

    #[test]
    fn left_docked_bar_tray_is_ignored() {
        // A vertical bar's tray sits inside the bar, left of the work area.
        let work = Rect {
            left: 48,
            top: 0,
            right: 3840,
            bottom: 1600,
        };
        let tray = Rect {
            left: 0,
            top: 1552,
            right: 48,
            bottom: 1600,
        };
        let r = place(&work, &MON, Some(tray), None, false, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(58, 1545, 500, 45));
        assert!(r.left >= work.left);
    }

    #[test]
    fn top_docked_bar_keeps_the_strip_on_the_desktop() {
        let mon = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work = Rect {
            left: 0,
            top: 48,
            right: 1920,
            bottom: 1080,
        };
        let tray = Rect {
            left: 1700,
            top: 0,
            right: 1920,
            bottom: 48,
        };
        let r = place(&work, &mon, Some(tray), None, false, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(MARGIN, 1025, 500, 45));
        assert!(r.top >= work.top);
    }

    #[test]
    fn hidden_bar_leaves_the_screen_edge_available() {
        // Auto-hidden bar: the work area covers the whole screen, and the
        // tray is not in a reserved bottom band.
        let mon = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let tray = Rect {
            left: 1600,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        let r = place(&mon, &mon, Some(tray), None, false, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(MARGIN, 1025, 500, 45));
    }

    #[test]
    fn empty_work_area_falls_back_to_the_monitor() {
        let work = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let r = place(&work, &MON, None, None, false, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(MARGIN, 1545, 500, 45));
    }

    #[test]
    fn narrow_desktop_still_gets_the_left_margin() {
        // Even a very narrow screen keeps the strip anchored at the left
        // margin; the strip is free to run toward the right edge.
        let mon = Rect {
            left: 0,
            top: 0,
            right: 600,
            bottom: 848,
        };
        let work = Rect {
            left: 0,
            top: 0,
            right: 600,
            bottom: 800,
        };
        let tray = Rect {
            left: 500,
            top: 800,
            right: 600,
            bottom: 848,
        };
        let r = place(&work, &mon, Some(tray), None, false, 500, 45, MARGIN);
        assert_eq!(r.left, MARGIN, "clamped to the left margin, not negative");
    }

    #[test]
    fn band_shorter_than_the_strip_top_aligns() {
        // A 30 px band cannot centre a 45 px strip: keep the top edge on
        // the band's top so the text baseline stays readable.
        let mon = Rect {
            left: 0,
            top: 0,
            right: 800,
            bottom: 772,
        };
        let work = Rect {
            left: 0,
            top: 0,
            right: 800,
            bottom: 742,
        };
        let tray = Rect {
            left: 700,
            top: 742,
            right: 800,
            bottom: 772,
        };
        let r = place(&work, &mon, Some(tray), None, false, 500, 45, MARGIN);
        assert_eq!(r.top, 742);
        assert_eq!(r.left, MARGIN);
    }

    #[test]
    fn obstacles_do_not_move_the_left_anchor() {
        // TrafficMonitor's taskbar window sits just left of the tray: the
        // strip is left-anchored, so in-band obstacles never move it.
        let obstacle = Rect {
            left: 3110,
            top: 1552,
            right: 3510,
            bottom: 1598,
        };
        let r = place(
            &BOTTOM_WORK,
            &MON,
            Some(BOTTOM_TRAY),
            Some(obstacle),
            false,
            500,
            45,
            MARGIN,
        );
        assert_eq!(r, Rect::new(MARGIN, 1553, 500, 45));
    }

    #[test]
    fn right_align_ends_left_of_the_tray() {
        let r = place(&BOTTOM_WORK, &MON, Some(BOTTOM_TRAY), None, true, 500, 45, MARGIN);
        // Right edge at 3526 - 10; band 1552..1600 is 48 px tall, the
        // strip is 45: centred at 1553.
        assert_eq!(r, Rect::new(3016, 1553, 500, 45));
        assert!(r.right <= BOTTOM_TRAY.left, "never over the tray");
        assert!(r.top >= BOTTOM_WORK.bottom, "inside the bar band");
    }

    #[test]
    fn right_align_without_a_tray_hugs_the_right_edge() {
        let r = place(&BOTTOM_WORK, &MON, None, None, true, 500, 45, MARGIN);
        assert_eq!(r, Rect::new(3330, 1497, 500, 45));
    }

    #[test]
    fn right_align_steps_left_of_an_in_band_obstacle() {
        // TrafficMonitor's taskbar window sits just left of the tray.
        let obstacle = Rect {
            left: 3110,
            top: 1552,
            right: 3510,
            bottom: 1598,
        };
        let r = place(
            &BOTTOM_WORK,
            &MON,
            Some(BOTTOM_TRAY),
            Some(obstacle),
            true,
            500,
            45,
            MARGIN,
        );
        assert_eq!(r.right, 3100, "right edge stops at the obstacle's left");
        assert!(r.right <= obstacle.left);
        assert_eq!(r.top, 1553, "still centred in the band");
    }

    #[test]
    fn right_align_keeps_the_tray_limit_when_there_is_no_room() {
        // Obstacle so close to the left edge that a 500 px strip cannot
        // fit to its left: fall back to the tray limit rather than going
        // off screen or clamping into the obstacle.
        let obstacle = Rect {
            left: 300,
            top: 1552,
            right: 700,
            bottom: 1598,
        };
        let r = place(
            &BOTTOM_WORK,
            &MON,
            Some(BOTTOM_TRAY),
            Some(obstacle),
            true,
            500,
            45,
            MARGIN,
        );
        assert_eq!(r.right, BOTTOM_TRAY.left - MARGIN);
    }

    #[test]
    fn right_align_ignores_an_obstacle_outside_the_band() {
        // A window above the taskbar must not affect the slot.
        let obstacle = Rect {
            left: 3110,
            top: 1000,
            right: 3510,
            bottom: 1500,
        };
        let r = place(
            &BOTTOM_WORK,
            &MON,
            Some(BOTTOM_TRAY),
            Some(obstacle),
            true,
            500,
            45,
            MARGIN,
        );
        assert_eq!(r, Rect::new(3016, 1553, 500, 45));
    }

    #[test]
    fn rect_helpers() {
        let r = Rect::new(5, 6, 7, 8);
        assert_eq!(r.width(), 7);
        assert_eq!(r.height(), 8);
        assert_eq!(r.right, 12);
        assert_eq!(r.bottom, 14);
    }
}
