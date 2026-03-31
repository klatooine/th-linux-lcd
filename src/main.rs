mod daemon;
mod gui;
mod shared;
mod usb;

fn main() {
    if std::env::args().any(|a| a == "--daemon") {
        daemon::run();
    } else {
        gui::run();
    }
}
