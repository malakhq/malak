//! Command-line entry point for the local Malak board.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod cli;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> std::process::ExitCode {
    cli::run()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> std::process::ExitCode {
    eprintln!("Error: Could not run the local board.\n  Next  Run Malak on Linux or macOS.");
    std::process::ExitCode::from(4)
}
