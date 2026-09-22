//! Which processes own visible windows, and what those windows are called.
//!
//! This is the difference between telling someone they have a process called
//! `vesktop` and telling them it is the Discord window they are looking at. No other
//! data source can make that connection.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub pid: i32,
    /// The application class, which is also what `.desktop` files match on via
    /// `StartupWMClass`.
    pub class: String,
    pub title: String,
    pub workspace: Option<String>,
    /// True for the window the user is currently looking at.
    pub focused: bool,
}

/// Every mapped window, or an empty list under a compositor we cannot query.
///
/// Window information is a bonus, never a requirement: the tool has to work the same
/// on a compositor with no IPC at all.
pub fn windows() -> Vec<Window> {
    hyprland_windows().unwrap_or_default()
}

/// Queries Hyprland over its control socket.
///
/// Deliberately talks to the socket rather than shelling out to `hyprctl`: this runs
/// on every refresh, and forking a process each time to read a few window titles
/// would be a self-inflicted cost.
fn hyprland_windows() -> Option<Vec<Window>> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let mut stream = UnixStream::connect(format!("{runtime}/hypr/{sig}/.socket.sock")).ok()?;
    stream.write_all(b"j/clients").ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;

    let value: serde_json::Value = serde_json::from_str(&buf).ok()?;
    let active = active_window_address(runtime.as_str(), sig.as_str());

    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|c| {
                let pid = c.get("pid")?.as_i64()? as i32;
                // Hyprland reports -1 for windows whose owner it cannot determine.
                if pid <= 0 {
                    return None;
                }
                Some(Window {
                    pid,
                    class: c.get("class")?.as_str().unwrap_or_default().to_string(),
                    title: c.get("title").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
                    workspace: c
                        .get("workspace")
                        .and_then(|w| w.get("name"))
                        .and_then(|n| n.as_str())
                        .map(str::to_string),
                    focused: active
                        .as_deref()
                        .is_some_and(|a| c.get("address").and_then(|x| x.as_str()) == Some(a)),
                })
            })
            .collect(),
    )
}

fn active_window_address(runtime: &str, sig: &str) -> Option<String> {
    let mut stream = UnixStream::connect(format!("{runtime}/hypr/{sig}/.socket.sock")).ok()?;
    stream.write_all(b"j/activewindow").ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    let v: serde_json::Value = serde_json::from_str(&buf).ok()?;
    v.get("address")?.as_str().map(str::to_string)
}
