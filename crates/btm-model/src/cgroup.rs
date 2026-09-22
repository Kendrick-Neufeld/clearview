//! Reading systemd's own opinion of what a process belongs to.

use serde::{Deserialize, Serialize};

/// What a cgroup v2 path tells us about a process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CgroupUnit {
    /// A graphical application launched through systemd, e.g.
    /// `app-vesktop-3929.scope`. The strongest grouping signal there is.
    AppScope { id: String, launcher_pid: Option<i32> },
    /// A managed service, e.g. `pipewire.service`.
    Service { unit: String, system: bool },
    /// The bare login session. Everything started outside systemd's knowledge
    /// lands here, which is why this can never be used as an app identity.
    Session,
    /// The root cgroup: kernel threads and anything unmanaged.
    Root,
    Other(String),
}

/// Reverses systemd's `\xNN` escaping of unit names.
pub fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x'
            && let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(v as char);
                i += 4;
                continue;
            }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Classifies a unified-hierarchy cgroup path.
pub fn classify(path: &str) -> CgroupUnit {
    if path == "/" || path.is_empty() {
        return CgroupUnit::Root;
    }
    let leaf = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    let system = path.starts_with("/system.slice");

    if let Some(name) = leaf.strip_suffix(".scope") {
        if let Some(rest) = name.strip_prefix("app-") {
            // `app-<id>-<pid>` for menu launches, `app-flatpak-<id>-<pid>` for
            // flatpaks. The trailing number is the launching pid, not part of the id.
            let rest = rest.strip_prefix("flatpak-").unwrap_or(rest);
            let (id, pid) = match rest.rsplit_once('-') {
                Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() => {
                    (head, tail.parse().ok())
                }
                _ => (rest, None),
            };
            return CgroupUnit::AppScope { id: unescape(id), launcher_pid: pid };
        }
        if name.starts_with("session-") {
            return CgroupUnit::Session;
        }
        return CgroupUnit::Other(unescape(name));
    }

    if let Some(unit) = leaf.strip_suffix(".service") {
        // Templated units carry an instance after `@` that is rarely meaningful
        // to a person.
        let unit = unit.split('@').next().unwrap_or(unit);
        return CgroupUnit::Service { unit: unescape(unit), system };
    }

    CgroupUnit::Other(unescape(leaf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_scopes_drop_the_launcher_pid() {
        let c = classify("/user.slice/user-1000.slice/user@1000.service/app.slice/app-vesktop-3929.scope");
        assert_eq!(c, CgroupUnit::AppScope { id: "vesktop".into(), launcher_pid: Some(3929) });
    }

    #[test]
    fn reverse_dns_ids_survive_the_pid_split() {
        let c = classify("/user.slice/.../app.slice/app-com.anthropic.Claude-18939.scope");
        assert_eq!(
            c,
            CgroupUnit::AppScope { id: "com.anthropic.Claude".into(), launcher_pid: Some(18939) }
        );
    }

    #[test]
    fn flatpak_prefix_is_not_part_of_the_id() {
        let c = classify("/user.slice/app.slice/app-flatpak-com.spotify.Client-4321.scope");
        assert_eq!(
            c,
            CgroupUnit::AppScope { id: "com.spotify.Client".into(), launcher_pid: Some(4321) }
        );
    }

    #[test]
    fn the_session_scope_is_never_an_app() {
        assert_eq!(classify("/user.slice/user-1000.slice/session-2.scope"), CgroupUnit::Session);
    }

    #[test]
    fn services_are_split_by_system_and_user() {
        assert_eq!(
            classify("/system.slice/avahi-daemon.service"),
            CgroupUnit::Service { unit: "avahi-daemon".into(), system: true }
        );
        assert_eq!(
            classify("/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service"),
            CgroupUnit::Service { unit: "pipewire".into(), system: false }
        );
    }

    #[test]
    fn escaped_unit_names_are_decoded() {
        assert_eq!(unescape(r"dbus\x2dbroker"), "dbus-broker");
    }

    #[test]
    fn kernel_threads_sit_in_the_root_cgroup() {
        assert_eq!(classify("/"), CgroupUnit::Root);
    }
}
