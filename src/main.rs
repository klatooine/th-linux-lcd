mod daemon;
mod gui;
mod tray;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--daemon") {
        std::process::exit(daemon::run());
    } else if args.iter().any(|a| a == "--tray") {
        std::process::exit(tray::run());
    } else {
        gui::run();
    }
}
