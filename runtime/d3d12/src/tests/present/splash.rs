use super::*;

// A retail package's ShellVisuals, with its attributes in the same order.
const PACKAGE: &str = r##"<Game configVersion="1">
  <ShellVisuals DefaultDisplayName="Sample Title" PublisherDisplayName="Sample Publisher"
    Square150x150Logo="logo_150.png" Description="Sample Title" ForegroundText="light"
    BackgroundColor="#000000" SplashScreenImage="splash.png" StoreLogo="logo_store.png" />
</Game>"##;

#[test]
fn reads_the_declared_image() {
    assert_eq!(declaration(PACKAGE).as_deref(), Some("splash.png"));
    let quoted = r#"<ShellVisuals SplashScreenImage='Art\Splash.png'/>"#;
    assert_eq!(declaration(quoted).as_deref(), Some(r"Art\Splash.png"));
}

#[test]
fn shows_nothing_without_a_declared_image() {
    let lanes = r#"<Game><Identity Name="ProjectTuxBox.Lanes"/><ExecutableList/></Game>"#;
    assert_eq!(declaration(lanes), None);
    assert_eq!(declaration(r##"<ShellVisuals BackgroundColor="#ffffff" />"##), None);
    assert_eq!(declaration(r#"<ShellVisuals SplashScreenImage="" />"#), None);
    // Attributes of other elements are not the splash's.
    assert_eq!(declaration(r#"<ShellVisuals/><Other SplashScreenImage="x.png"/>"#), None);
}

fn edges(at: RECT) -> (i32, i32, i32, i32) {
    (at.left, at.top, at.right, at.bottom)
}

#[test]
fn the_image_fills_a_window_of_its_proportions() {
    // A 2560x1440 window showing 1920x1080 art, as reported with black borders.
    assert_eq!(edges(placement((2560, 1440), (1920, 1080))), (0, 0, 2560, 1440));
    assert_eq!(edges(placement((3840, 2160), (1920, 1080))), (0, 0, 3840, 2160));
    assert_eq!(edges(placement((1920, 1080), (1920, 1080))), (0, 0, 1920, 1080));
}

#[test]
fn other_proportions_keep_the_image_s_own_between_bars() {
    // A work area below a 36 pixel top bar.
    assert_eq!(edges(placement((2560, 1404), (1920, 1080))), (32, 0, 2528, 1404));
    assert_eq!(edges(placement((1920, 1044), (1920, 1080))), (32, 0, 1888, 1044));
    assert_eq!(edges(placement((1000, 1000), (1920, 1080))), (0, 219, 1000, 781));
    assert_eq!(edges(placement((1000, 400), (1920, 1080))), (144, 0, 855, 400));
    let empty = placement((0, 0), (1920, 1080));
    assert_eq!((empty.right - empty.left, empty.bottom - empty.top), (0, 0));
}

#[test]
fn enlarging_interpolates_between_samples_and_keeps_the_ends() {
    let row = [0, 0, 0, 0, 200, 200, 200, 200];
    let wide = resize(&row, (2, 1), (4, 1)).unwrap();
    assert_eq!(wide.chunks_exact(4).map(|pixel| pixel[0]).collect::<Vec<_>>(), [0, 50, 150, 200]);
    assert_eq!(resize(&[7, 8, 9, 255], (1, 1), (3, 2)).unwrap(), [7, 8, 9, 255].repeat(6));
}

#[test]
fn shrinking_averages_the_area_each_pixel_covers() {
    // Premultiplied BGRA: 4x2 to 2x1 averages each 2x2 block.
    let pixels = [
        10, 20, 30, 255, 30, 40, 50, 255, /**/ 0, 0, 0, 0, 0, 0, 0, 0, //
        50, 60, 70, 255, 70, 80, 90, 255, /**/ 100, 100, 100, 128, 0, 0, 0, 0,
    ];
    assert_eq!(resize(&pixels, (4, 2), (2, 1)).unwrap(), [40, 50, 60, 255, 25, 25, 25, 32]);
    // Three samples to two: each output covers one and a half inputs.
    let row = [0, 0, 0, 0, 90, 90, 90, 90, 180, 180, 180, 180];
    assert_eq!(resize(&row, (3, 1), (2, 1)).unwrap(), [30, 30, 30, 30, 150, 150, 150, 150]);
}

#[test]
fn shrinking_keeps_a_flat_image_flat() {
    // The 1920x1080 art on a 1920x1080 monitor's work area.
    let pixels: Vec<u8> = [48, 192, 144, 255].repeat(1920 * 1080);
    let shrunk = resize(&pixels, (1920, 1080), (1856, 1044)).unwrap();
    assert_eq!(shrunk.len(), 1856 * 1044 * 4);
    assert!(shrunk.chunks_exact(4).all(|pixel| pixel == [48, 192, 144, 255]));
}

#[test]
fn isolated_frames_stay_behind_the_splash() {
    // ProjectScarlett: one frame about 2 s in, another some seconds later.
    let start = Instant::now();
    let mut pacing = Pacing::default();
    for at in [1.8, 2.3, 20.0, 20.9] {
        assert_eq!(pacing.arrive(start + Duration::from_secs_f64(at)), Verdict::Hold, "{at}");
    }
}

#[test]
fn steady_frames_end_it() {
    let start = Instant::now();
    let mut pacing = Pacing::default();
    assert_eq!(pacing.arrive(start + Duration::from_secs_f64(1.8)), Verdict::Hold);
    assert_eq!(pacing.arrive(start + Duration::from_secs_f64(19.0)), Verdict::Hold);
    for step in 0..3 {
        let at = start + Duration::from_secs(26) + Duration::from_millis(6 * step);
        assert_eq!(pacing.arrive(at), Verdict::Hold);
    }
    // The fourth frame within a second.
    assert_eq!(pacing.arrive(start + Duration::from_millis(26_018)), Verdict::Steady);
    assert_eq!(pacing.frames, 6);
}

#[test]
fn a_title_that_never_presents_steadily_still_reaches_the_screen() {
    let start = Instant::now();
    let mut pacing = Pacing::default();
    for second in (0..60).step_by(2) {
        assert_eq!(pacing.arrive(start + Duration::from_secs(second)), Verdict::Hold, "{second}");
    }
    assert_eq!(pacing.arrive(start + Duration::from_secs(60)), Verdict::Timeout);
}
