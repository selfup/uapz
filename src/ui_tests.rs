//! Native Metal rendering and workspace navigation regression harness.
//! Run with `cargo test --features ui-tests --test workspace_rendering` on macOS.
//! Set UAPZ_UI_SCREENSHOT_DIR to choose the screenshot output directory.
// This standalone harness shares application modules but runs its own main;
// their ordinary #[test] functions are omitted while cfg(test) helpers remain.
#![allow(dead_code, unused_imports)]

mod calls;
mod doc;
mod exec;
mod graph;
mod history;
mod inspector;
mod library;
mod node_data;
mod providers;
mod review;
mod sidebar;
mod storage;
mod validate;
mod workflow;

fn main() {
    #[cfg(target_os = "macos")]
    sidebar::rendering_tests::run();
    #[cfg(not(target_os = "macos"))]
    println!("workspace_rendering: Metal headless rendering requires macOS");
}
