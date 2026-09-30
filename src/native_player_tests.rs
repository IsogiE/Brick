//! Runs each existing native assertion on the process main thread.
//! This target shares the app graph but only calls the requested isolated test.
// Sharing the app graph also compiles helpers unrelated to the selected case.
#![allow(dead_code, unused_imports)]

include!("app_modules.rs");

fn main() {
    let mut args = std::env::args().skip(1);
    let case = args
        .next()
        .expect("Choose resize, overlays, fullscreen, or content");
    assert!(args.next().is_none(), "Run one native case per process");
    match case.as_str() {
        "resize" => stream_player::resize::tests::run_native_resize(),
        "overlays" => stream_player::occlusion::native_tests::run_native_overlays(),
        "fullscreen" => stream_player::fullscreen::tests::run_native_fullscreen(),
        "content" => review_ui::content_review::native_smoke::run_native_content(),
        _ => panic!("Unknown native case: {case}"),
    }
    println!("Native player {case}: passed");
}
