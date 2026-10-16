mod daemon;
mod gui;

fn main() {
    if std::env::args().any(|a| a == "--daemon") {
        std::process::exit(daemon::run());
    } else {
        gui::run();
    }
}
