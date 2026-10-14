// The whole app: identity bakes from THIS crate's tauri.conf.json via
// generate_context!; behavior is the shared shell library.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    ph_desktop_shell::run(tauri::generate_context!(), Default::default());
}
