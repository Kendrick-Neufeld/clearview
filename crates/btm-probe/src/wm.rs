//! Which processes own visible windows, and what those windows are called.
//!
//! This is the difference between telling someone they have a process called
//! `vesktop` and telling them it is the Discord window they are looking at. No other
//! data source can make that connection.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

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
/// Window information is never strictly required, but losing it is not
/// harmless: which process owns a window is what separates an application from
/// a background process, and what stops a program launched by the compositor
/// from being filed under the compositor. Under a compositor this cannot
/// query, everything falls back to weaker signals — so supporting more of them
/// matters more than it looks.
pub fn windows() -> Vec<Window> {
    hyprland_windows()
        .or_else(niri_windows)
        .unwrap_or_default()
}

/// Which compositor answered, for the interface to report when none did.
pub fn source() -> Option<&'static str> {
    if hyprland_socket().is_some() {
        Some("Hyprland")
    } else if niri_socket().is_some() {
        Some("niri")
    } else {
        None
    }
}

/// The directory the compositor keeps its sockets in.
///
/// `XDG_RUNTIME_DIR` is normally set, but a process started from a launcher or
/// a service can inherit an environment without it, so the conventional path
/// stands in.
fn runtime_dir() -> Option<PathBuf> {
    let from_env = std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from);
    let conventional = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    from_env.filter(|p| p.is_dir()).or(Some(conventional).filter(|p| p.is_dir()))
}

/// Finds Hyprland's control socket.
///
/// The instance signature is normally in the environment, but only for
/// processes descended from the compositor. Launched another way — from a
/// service, or a launcher with a trimmed environment — that variable is absent,
/// and relying on it alone meant the window list came back empty. Nothing
/// failed visibly; every application simply lost its windows and was filed as a
/// background process, which looks like a classification bug rather than a
/// missing environment variable.
///
/// So the signature is a hint, not a requirement: failing that, the sockets are
/// found on disk, most recent first.
fn hyprland_socket() -> Option<PathBuf> {
    let runtime = runtime_dir()?;
    let hypr = runtime.join("hypr");

    if let Ok(signature) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        let path = hypr.join(&signature).join(".socket.sock");
        if path.exists() {
            return Some(path);
        }
    }

    // No usable signature. Every instance leaves a directory here; take the
    // most recently created one, which is the running session in the ordinary
    // case of exactly one.
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&hypr)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let socket = entry.path().join(".socket.sock");
            if !socket.exists() {
                return None;
            }
            let when = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((when, socket))
        })
        .collect();

    candidates.sort_by_key(|(when, _)| *when);
    candidates.pop().map(|(_, socket)| socket)
}

fn ask(request: &[u8]) -> Option<String> {
    let mut stream = UnixStream::connect(hyprland_socket()?).ok()?;
    stream.write_all(request).ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Queries Hyprland over its control socket.
///
/// Deliberately talks to the socket rather than shelling out to `hyprctl`: this
/// runs on every refresh, and forking a process each time to read a few window
/// titles would be a self-inflicted cost.
fn hyprland_windows() -> Option<Vec<Window>> {
    let value: serde_json::Value = serde_json::from_str(&ask(b"j/clients")?).ok()?;
    let active = active_window_address();

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

fn active_window_address() -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&ask(b"j/activewindow")?).ok()?;
    v.get("address")?.as_str().map(str::to_string)
}

/// Whether window information is obtainable at all. The interface can say so
/// rather than silently presenting every application as a background process.
pub fn available() -> bool {
    source().is_some()
}

/// Finds niri's control socket.
///
/// `NIRI_SOCKET` names it directly for processes niri started. As with
/// Hyprland, that variable is absent from a trimmed environment, so the
/// runtime directory is searched as well.
fn niri_socket() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("NIRI_SOCKET") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let runtime = runtime_dir()?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&runtime)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            // niri names its socket `niri.<wayland display>.<pid>.sock`.
            if !name.starts_with("niri") || !name.ends_with(".sock") {
                return None;
            }
            let when = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((when, entry.path()))
        })
        .collect();

    candidates.sort_by_key(|(when, _)| *when);
    candidates.pop().map(|(_, socket)| socket)
}

/// Queries niri over its control socket.
///
/// niri speaks newline-delimited JSON: one request per line, one reply per
/// line. The reply is `{"Ok": {"Windows": [...]}}`.
fn niri_windows() -> Option<Vec<Window>> {
    let mut stream = UnixStream::connect(niri_socket()?).ok()?;
    stream.write_all(b"\"Windows\"\n").ok()?;
    stream.flush().ok()?;

    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;

    let reply: serde_json::Value = serde_json::from_str(buf.lines().next()?).ok()?;
    let list = reply.get("Ok")?.get("Windows")?.as_array()?;

    Some(
        list.iter()
            .filter_map(|w| {
                // Older niri builds do not report a pid; without one a window
                // cannot be attributed, so it is skipped rather than guessed at.
                let pid = w.get("pid")?.as_i64()? as i32;
                if pid <= 0 {
                    return None;
                }
                Some(Window {
                    pid,
                    class: w.get("app_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    title: w.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    workspace: w.get("workspace_id").map(|v| v.to_string()),
                    focused: w.get("is_focused").and_then(|v| v.as_bool()).unwrap_or(false),
                })
            })
            .collect(),
    )
}
