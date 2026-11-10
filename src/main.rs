mod app;
mod audio;
mod check;
mod cmlight;
mod environment;
mod envscene;
mod eval;
mod gpu3d;
mod groups;
mod map;
mod motion;
mod preview;
mod refrender;
mod scene;
mod v4;
mod view3d;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--check") {
        let Some(dir) = args.get(1) else {
            eprintln!("usage: bs-light-timeline --check <CustomLevels folder> [--audio]");
            std::process::exit(2);
        };
        let audio = args.iter().any(|a| a == "--audio");
        std::process::exit(check::run(std::path::Path::new(dir), audio));
    }
    if args.first().map(String::as_str) == Some("--save-test") {
        let (Some(src), Some(work)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: bs-light-timeline --save-test <map folder> <work dir>");
            std::process::exit(2);
        };
        std::process::exit(check::save_test(
            std::path::Path::new(src),
            std::path::Path::new(work),
        ));
    }
    if args.first().map(String::as_str) == Some("--render") && args.len() >= 5 {
        std::process::exit(check::render(
            std::path::Path::new(&args[1]),
            &args[2],
            &args[3],
            std::path::Path::new(&args[4]),
            args.get(5).map(String::as_str),
        ));
    }
    if args.first().map(String::as_str) == Some("--render-ref") && args.len() >= 2 {
        std::process::exit(refrender::run(std::path::Path::new(&args[1]), args.get(2).map(String::as_str)));
    }
    if args.first().map(String::as_str) == Some("--plan-compare") && args.len() >= 3 {
        std::process::exit(check::plan_compare(std::path::Path::new(&args[1]), std::path::Path::new(&args[2])));
    }
    if args.first().map(String::as_str) == Some("--compare-motion") && args.len() >= 4 {
        std::process::exit(check::compare_motion(std::path::Path::new(&args[1]), &args[2], std::path::Path::new(&args[3])));
    }
    if args.first().map(String::as_str) == Some("--compare-lights") && args.len() >= 4 {
        std::process::exit(check::compare_lights(
            std::path::Path::new(&args[1]),
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[3]),
        ));
    }
    let map_folder = args.into_iter().next();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 900.0])
            .with_title("Beat Saber Light Timeline"),
        ..Default::default()
    };
    eframe::run_native(
        "Beat Saber Light Timeline",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(map_folder, cc.wgpu_render_state.clone())))),
    )
}
