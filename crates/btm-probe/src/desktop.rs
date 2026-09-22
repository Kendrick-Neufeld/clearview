//! The freedesktop application database — the only place that knows a binary called
//! `zen-bin` is the thing the user calls "Zen Browser", and which icon to draw.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopEntry {
    /// The `foo` in `foo.desktop`; for most modern apps this is a reverse-DNS id.
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    /// Matches the window class the app sets at runtime.
    pub wm_class: Option<String>,
    /// Basename of the first token of `Exec`, with field codes stripped.
    pub exec_name: Option<String>,
    pub categories: Vec<String>,
    /// Entries flagged `NoDisplay` are helpers and shortcuts, not things a user
    /// thinks of as applications.
    pub no_display: bool,
}

/// An index over the system's desktop entries, built once and reused.
#[derive(Debug, Default, Clone)]
pub struct DesktopDb {
    entries: Vec<DesktopEntry>,
    by_wm_class: HashMap<String, usize>,
    by_exec: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
}

impl DesktopDb {
    /// Scans `$XDG_DATA_DIRS` and `$XDG_DATA_HOME`. Later directories take
    /// precedence, so a user's own entry overrides the system one.
    pub fn load() -> Self {
        let mut db = DesktopDb::default();
        for dir in search_dirs() {
            let Ok(read) = fs::read_dir(&dir) else { continue };
            for file in read.flatten() {
                let path = file.path();
                if path.extension().is_some_and(|e| e == "desktop")
                    && let Some(entry) = parse_entry(&path)
                {
                    db.insert(entry);
                }
            }
        }
        db
    }

    fn insert(&mut self, entry: DesktopEntry) {
        let idx = self.entries.len();
        self.by_id.insert(entry.id.to_lowercase(), idx);
        if let Some(c) = &entry.wm_class {
            self.by_wm_class.insert(c.to_lowercase(), idx);
        }
        if let Some(e) = &entry.exec_name {
            // Never let a NoDisplay helper claim an executable name away from a
            // real application — Steam shortcuts alone would hijack dozens.
            let slot = self.by_exec.entry(e.to_lowercase());
            match slot {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(idx);
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    if self.entries[*o.get()].no_display && !entry.no_display {
                        o.insert(idx);
                    }
                }
            }
        }
        self.entries.push(entry);
    }

    /// Best match for a window class, as reported by the compositor.
    pub fn by_wm_class(&self, class: &str) -> Option<&DesktopEntry> {
        let key = class.to_lowercase();
        self.by_wm_class
            .get(&key)
            .or_else(|| self.by_id.get(&key))
            .or_else(|| self.by_exec.get(&key))
            .map(|i| &self.entries[*i])
    }

    /// Best match for an executable name, e.g. `zen-bin`.
    pub fn by_exec_name(&self, exec: &str) -> Option<&DesktopEntry> {
        let key = exec.to_lowercase();
        self.by_exec
            .get(&key)
            .or_else(|| self.by_id.get(&key))
            .map(|i| &self.entries[*i])
    }

    /// Match on the identifier systemd embeds in an `app-<id>-<pid>.scope`, which is
    /// usually the desktop entry id for anything launched from a menu.
    pub fn by_id(&self, id: &str) -> Option<&DesktopEntry> {
        self.by_id.get(&id.to_lowercase()).map(|i| &self.entries[*i])
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(Path::new(d).join("applications"));
    }
    let home_data = std::env::var("XDG_DATA_HOME").ok().map(PathBuf::from).or_else(|| {
        std::env::var("HOME").ok().map(|h| Path::new(&h).join(".local/share"))
    });
    if let Some(h) = home_data {
        dirs.push(h.join("applications"));
    }
    dirs
}

/// Reads the `[Desktop Entry]` group only. Action groups below it describe extra
/// launcher shortcuts ("New Window") and would otherwise overwrite the real name.
fn parse_entry(path: &Path) -> Option<DesktopEntry> {
    let text = fs::read_to_string(path).ok()?;
    let id = path.file_stem()?.to_str()?.to_string();
    let mut e = DesktopEntry {
        id,
        name: String::new(),
        icon: None,
        wm_class: None,
        exec_name: None,
        categories: Vec::new(),
        no_display: false,
    };

    let mut in_main_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_group || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            // Ignore localised variants like `Name[de]`; we want the default.
            "Name" if e.name.is_empty() => e.name = value.trim().to_string(),
            "Icon" => e.icon = Some(value.trim().to_string()),
            "StartupWMClass" => e.wm_class = Some(value.trim().to_string()),
            "Exec" if e.exec_name.is_none() => e.exec_name = exec_basename(value),
            "Categories" => {
                e.categories =
                    value.split(';').filter(|s| !s.is_empty()).map(str::to_string).collect()
            }
            "NoDisplay" | "Hidden" => e.no_display |= value.trim() == "true",
            _ => {}
        }
    }

    (!e.name.is_empty()).then_some(e)
}

/// Extracts the program name from an `Exec=` line, discarding wrappers, environment
/// prefixes and the `%U`-style field codes.
fn exec_basename(exec: &str) -> Option<String> {
    let mut tokens = exec.split_ascii_whitespace().peekable();
    // `env VAR=x prog` and `sh -c prog` wrappers hide the real binary.
    if tokens.peek() == Some(&"env") {
        tokens.next();
        while tokens.peek().is_some_and(|t| t.contains('=')) {
            tokens.next();
        }
    }
    let first = tokens.next()?.trim_matches('\'').trim_matches('"');
    let base = Path::new(first).file_name()?.to_str()?;
    (!base.starts_with('%')).then(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_basename_strips_paths_and_field_codes() {
        assert_eq!(exec_basename("/usr/lib/firefox/firefox %u").as_deref(), Some("firefox"));
        assert_eq!(exec_basename("claude-desktop %U").as_deref(), Some("claude-desktop"));
        assert_eq!(exec_basename("'/home/b/game' %u").as_deref(), Some("game"));
        assert_eq!(exec_basename("env LANG=C /usr/bin/foo").as_deref(), Some("foo"));
    }
}
