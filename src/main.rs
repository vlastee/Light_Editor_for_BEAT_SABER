mod app;
mod audio;
mod check;
mod eval;
mod map;
mod preview;
mod scene;
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
        let diff = args[2].parse().unwrap_or(0);
        let beat = args[3].parse().unwrap_or(0.0);
        std::process::exit(check::render(
            std::path::Path::new(&args[1]),
            diff,
            beat,
            std::path::Path::new(&args[4]),
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
        Box::new(|_cc| Ok(Box::new(app::App::new(map_folder)))),
    )
}
