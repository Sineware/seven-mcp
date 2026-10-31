// seven mcp: an mcp server for windows 7, in the notification area
#![windows_subsystem = "windows"]

mod auth;
mod http;
mod mcp;
mod settings;
mod tools;
mod ui;

fn main() {
    let port = settings::get("Port").and_then(|p| p.parse().ok()).unwrap_or(7777);
    let Some(listener) = http::listen(port) else {
        return ui::error(&format!("Seven MCP can't use port {port} because it is already in use."));
    };
    ui::run(port, move || http::serve(listener));
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
