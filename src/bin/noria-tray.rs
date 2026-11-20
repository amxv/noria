#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "../tray/mod.rs"]
mod tray;

fn main() {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if let Err(error) = tray::main() {
        eprintln!("Noria tray: {error:#}");
        std::process::exit(1);
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        eprintln!("macOS uses the native Swift Noria.app");
        std::process::exit(1);
    }
}
