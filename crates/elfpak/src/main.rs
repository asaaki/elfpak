//! Wrapper around the `elfpak` library, which holds the command line interface
//! itself.

fn main() -> std::process::ExitCode {
    elfpak::run()
}
