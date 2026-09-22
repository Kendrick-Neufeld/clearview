//! Deciding which application a process belongs to.
//!
//! No single source gets this right. systemd's cgroups are authoritative when they
//! apply, but processes that daemonise or are spawned outside their launcher's scope
//! fall out into the bare session scope — on this machine, six of Vesktop's eight
//! processes do exactly that. Grouping by process name instead is worse: it merges
//! unrelated programs and splits apps whose helpers are named differently.
//!
//! So the resolver tries sources in order of trustworthiness and records which one
//! answered, so the interface can be honest about how sure it is.

use crate::cgroup::{self, CgroupUnit};
use btm_probe::ProcSample;
use btm_probe::desktop::DesktopDb;
use btm_probe::wm::Window;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How an app was identified, weakest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentitySource {
    /// systemd put it in a named application scope.
    AppScope,
    /// It owns a window, and the compositor named it.
    Window,
    /// An ancestor was identified and this process inherited that identity.
    Ancestry,
    /// It is a managed background service.
    SystemdService,
    /// Matched against the freedesktop application database by executable.
    DesktopEntry,
    /// Nothing matched; falling back to the program's own name.
    Executable,
    Kernel,
}

impl IdentitySource {
    /// Whether this source is strong enough to state without qualification.
    pub fn is_certain(self) -> bool {
        matches!(self, IdentitySource::AppScope | IdentitySource::Window | IdentitySource::Kernel)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Grouping key. Every process sharing this belongs to one app.
    pub app_id: String,
    pub source: IdentitySource,
}

pub struct Resolver<'a> {
    procs: HashMap<i32, &'a ProcSample>,
    windows_by_pid: HashMap<i32, Vec<&'a Window>>,
    db: &'a DesktopDb,
}

impl<'a> Resolver<'a> {
    pub fn new(procs: &'a [ProcSample], windows: &'a [Window], db: &'a DesktopDb) -> Self {
        let mut windows_by_pid: HashMap<i32, Vec<&Window>> = HashMap::new();
        for w in windows {
            windows_by_pid.entry(w.pid).or_default().push(w);
        }
        Resolver {
            procs: procs.iter().map(|p| (p.key.pid, p)).collect(),
            windows_by_pid,
            db,
        }
    }

    pub fn windows_for(&self, pid: i32) -> &[&'a Window] {
        self.windows_by_pid.get(&pid).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Identity this process carries on its own, ignoring its ancestors.
    fn own_identity(&self, p: &ProcSample) -> Option<Identity> {
        if p.is_kernel_thread() {
            return Some(Identity { app_id: "kernel".into(), source: IdentitySource::Kernel });
        }

        let unit = p.cgroup.as_deref().map(cgroup::classify);
        let scope_id = match &unit {
            Some(CgroupUnit::AppScope { id, .. }) => Some(id.clone()),
            _ => None,
        };

        // A process that owns a window states its own identity. This outranks an
        // inherited app scope, because launching one app from another — a browser
        // from a terminal, say — leaves the child sitting in the parent's scope,
        // where it would otherwise be filed under the wrong program entirely.
        if let Some(w) = self.windows_for(p.key.pid).first() {
            let class_matches =
                scope_id.as_deref().is_some_and(|id| ids_agree(id, &w.class));
            if !class_matches {
                return Some(Identity {
                    app_id: w.class.clone(),
                    source: IdentitySource::Window,
                });
            }
        }

        match unit {
            Some(CgroupUnit::AppScope { id, .. }) => {
                Some(Identity { app_id: id, source: IdentitySource::AppScope })
            }
            Some(CgroupUnit::Service { unit, .. }) => {
                Some(Identity { app_id: unit, source: IdentitySource::SystemdService })
            }
            // The session scope and the root cgroup say nothing about identity.
            _ => None,
        }
    }

    /// Full resolution, walking up the process tree when the process itself is
    /// anonymous.
    pub fn resolve(&self, p: &ProcSample) -> Identity {
        if let Some(id) = self.own_identity(p) {
            return id;
        }

        // Walk towards init looking for an ancestor that does know what it is. The
        // depth cap and the visited set guard against a corrupted or racing
        // `/proc` producing a cycle.
        //
        // Only an application scope or a window may be inherited. A *service*
        // ancestor must not be, because the login manager and the user's systemd
        // instance are ancestors of the entire desktop — inheriting from them files
        // every window the user has open under `sddm`, which is how this went wrong
        // the first time.
        let mut seen = Vec::new();
        let mut cur = p.ppid;
        for _ in 0..32 {
            if cur <= 1 || seen.contains(&cur) {
                break;
            }
            seen.push(cur);
            let Some(parent) = self.procs.get(&cur) else { break };
            match self.own_identity(parent) {
                Some(found)
                    if matches!(
                        found.source,
                        IdentitySource::AppScope | IdentitySource::Window
                    ) =>
                {
                    return Identity { app_id: found.app_id, source: IdentitySource::Ancestry };
                }
                // A service owns itself, not its session's descendants.
                Some(_) => break,
                None => {}
            }
            cur = parent.ppid;
        }

        // Nothing above it knows either. Fall back to the program itself.
        let exe = executable_name(p);
        if let Some(entry) = self.db.by_exec_name(&exe) {
            return Identity { app_id: entry.id.clone(), source: IdentitySource::DesktopEntry };
        }
        Identity { app_id: exe, source: IdentitySource::Executable }
    }
}

/// The program's own name, preferring the full path in its argv over the kernel's
/// 15-character truncation of it.
pub fn executable_name(p: &ProcSample) -> String {
    p.cmdline
        .first()
        .and_then(|a| a.split_ascii_whitespace().next())
        .and_then(|a| a.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(&p.comm)
        .to_string()
}

/// Whether a cgroup scope id and a window class refer to the same application.
/// Compared loosely because one is a unit name and the other is chosen by the app.
fn ids_agree(scope_id: &str, wm_class: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().replace(['-', '_', '.'], "");
    let (a, b) = (norm(scope_id), norm(wm_class));
    a == b || a.ends_with(&b) || b.ends_with(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ids_and_window_classes_are_matched_loosely() {
        assert!(ids_agree("vesktop", "vesktop"));
        assert!(ids_agree("com.anthropic.Claude", "com.anthropic.Claude"));
        // systemd's id and the app's own class often differ in case and separators.
        assert!(ids_agree("org.mozilla.firefox", "firefox"));
        assert!(!ids_agree("vesktop", "kitty"));
    }
}
