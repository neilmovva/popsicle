#[cfg(target_os = "linux")]
include!("linux.rs");

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(macos::main());
}
