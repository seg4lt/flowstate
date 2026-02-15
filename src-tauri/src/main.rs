// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("server") {
        std::process::exit(flowzen_lib::server_entry::run(argv));
    }
    flowzen_lib::run()
}
