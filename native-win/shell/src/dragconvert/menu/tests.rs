// Wheel geometry tests: sector hit calculation, angle normalization, frame
// clamping and label titles.

use super::*;

fn point_on(angle: f64, radius: f64) -> (f64, f64) {
    let (cx, cy) = CENTER;
    (cx + angle.sin() * radius, cy - angle.cos() * radius)
}

#[test]
fn sector_angles_cover_the_compass() {
    let tau = std::f64::consts::TAU;
    assert!((sector_angle(126.0, 20.0)).abs() < 1e-9, "top is zero");
    assert!((sector_angle(220.0, 126.0) - tau / 4.0).abs() < 1e-9, "right is a quarter turn");
    assert!((sector_angle(126.0, 232.0) - tau / 2.0).abs() < 1e-9, "bottom is half");
    assert!((sector_angle(32.0, 126.0) - tau * 0.75).abs() < 1e-9, "left is three quarters");
}

#[test]
fn sector_hit_matches_each_sector_center() {
    for count in 3..=MAXIMUM_SECTORS {
        let span = std::f64::consts::TAU / count as f64;
        for index in 0..count {
            let angle = index as f64 * span;
            let (x, y) = point_on(angle, (INNER_RADIUS + OUTER_RADIUS) / 2.0);
            assert_eq!(sector_hit(x, y, count), Some(index), "count={count} index={index}");
        }
    }
}

#[test]
fn sector_hit_rejects_center_gaps_and_edges() {
    let count = 9;
    // Center dot and inner hub are neutral.
    assert_eq!(sector_hit(CENTER.0, CENTER.1, count), None);
    assert_eq!(sector_hit(CENTER.0, CENTER.1 - 20.0, count), None);
    // Outside the wheel is neutral.
    assert_eq!(sector_hit(CENTER.0, CENTER.1 - 200.0, count), None);
    let span = std::f64::consts::TAU / count as f64;
    // Sector gaps (boundaries between sectors) cancel.
    let boundary = span / 2.0;
    let (x, y) = point_on(boundary, (INNER_RADIUS + OUTER_RADIUS) / 2.0);
    assert_eq!(sector_hit(x, y, count), None, "gutter release is a cancel");
    // Radii just outside the annulus are neutral too.
    let (x, y) = point_on(0.0, INNER_RADIUS - 1.0);
    assert_eq!(sector_hit(x, y, count), None);
    let (x, y) = point_on(0.0, OUTER_RADIUS + 1.0);
    assert_eq!(sector_hit(x, y, count), None);
    // Degenerate sector counts never hit.
    assert_eq!(sector_hit(126.0, 40.0, 2), None);
    assert_eq!(sector_hit(126.0, 40.0, 10), None);
    assert_eq!(sector_hit(126.0, 40.0, 0), None);
}

#[test]
fn label_points_face_outward_per_sector() {
    let (x, y) = label_point(0, 6);
    assert!((x - CENTER.0).abs() < 1e-9, "first sector labels at twelve o'clock");
    assert!(y < CENTER.1);
    let (x, _y) = label_point(1, 4);
    assert!(x > CENTER.0, "clockwise ordering");
}

#[test]
fn frame_at_clamps_to_the_work_area() {
    let work = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };
    assert_eq!(frame_at((960, 500), work), (960 - SIDE / 2, 500 - SIDE / 2));
    // Screen edges clamp with an 8 px margin.
    assert_eq!(frame_at((2, 2), work), (8, 8));
    assert_eq!(frame_at((2000, 2000), work), (1920 - SIDE - 8, 1040 - SIDE - 8));
    // Tiny work areas stay within bounds.
    let tiny = RECT { left: 100, top: 100, right: 200, bottom: 200 };
    let (x, y) = frame_at((150, 150), tiny);
    assert!(x >= tiny.left + 8 && y >= tiny.top + 8);
}

#[test]
fn action_titles_match_the_lab_preview() {
    assert_eq!(action_title("zip", false), "打包 ZIP");
    assert_eq!(action_title("zip", true), "Pack ZIP");
    assert_eq!(action_title("small", false), "小图 JPG");
    assert_eq!(action_title("imagesPDF", true), "To PDF");
    assert_eq!(action_title("tarGz", true), "TAR.GZ");
    assert_eq!(action_title("splitPDF", false), "拆分页面");
    assert_eq!(action_title("heic", true), "HEIC");
}
