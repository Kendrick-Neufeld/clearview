//! Acting on a process, rather than only watching it.
//!
//! The one place this crate does something irreversible, so it is also the one
//! place with a guard: every signal is addressed to a process *identity*, not a
//! number.

use crate::process;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// Ask the program to close. It can save its work and shut down properly.
    Terminate,
    /// Take it away from the program. Nothing is saved.
    Force,
}

impl Signal {
    fn as_raw(self) -> i32 {
        match self {
            Signal::Terminate => libc::SIGTERM,
            Signal::Force => libc::SIGKILL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalError {
    /// The process is gone, or the pid now belongs to something else.
    Vanished,
    /// Someone else's process, or one the kernel protects.
    NotPermitted,
    /// Refused here rather than by the kernel.
    Refused(String),
    Other(String),
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalError::Vanished => write!(f, "that process has already exited"),
            SignalError::NotPermitted => {
                write!(f, "not allowed — it belongs to another user or to the system")
            }
            SignalError::Refused(why) => write!(f, "{why}"),
            SignalError::Other(why) => write!(f, "{why}"),
        }
    }
}

/// Compositors and display servers. Ending one of these does not close a
/// program; it ends the graphical session, taking every window with it and
/// usually requiring a reboot to recover from.
///
/// Xwayland is deliberately absent: it can be killed and restarted, and doing
/// so is sometimes useful. These cannot.
const COMPOSITORS: [&str; 14] = [
    "Hyprland", "niri", "sway", "river", "weston", "labwc", "wayfire",
    "kwin_wayland", "kwin_x11", "mutter", "gnome-shell", "plasmashell",
    "cosmic-comp", "Xorg",
];

/// Whether `pid` is an ancestor of this very process.
///
/// Signalling an ancestor kills whatever started us, and usually us with it.
/// This is the general form of the problem: it catches the compositor, the
/// login session, the user's systemd instance and the terminal, without
/// needing to recognise any of them by name.
fn is_ancestor_of_self(pid: i32) -> bool {
    let mut current = std::process::id() as i32;
    for _ in 0..64 {
        let Some(stat) = process::read_stat(current) else { return false };
        if stat.ppid == pid {
            return true;
        }
        if stat.ppid <= 1 {
            return false;
        }
        current = stat.ppid;
    }
    false
}

/// Whether this process is the compositor running the desktop.
fn is_compositor(comm: &str) -> bool {
    COMPOSITORS.iter().any(|c| c.eq_ignore_ascii_case(comm))
}

/// Sends a signal, but only if the pid still refers to the same process.
///
/// Pids are reused. Between the moment a list is drawn and the moment someone
/// clicks, the process can exit and its number be handed to something else —
/// so a signal sent by number alone can land on an innocent bystander. The
/// start time is re-read immediately before signalling and must still match the
/// one the caller saw; if it does not, nothing is sent.
pub fn send_signal(
    pid: i32,
    expected_start_time: u64,
    signal: Signal,
) -> Result<(), SignalError> {
    if pid <= 1 {
        return Err(SignalError::Refused(
            "init keeps the system running and cannot be signalled".into(),
        ));
    }

    let Some(stat) = process::read_stat(pid) else {
        return Err(SignalError::Vanished);
    };
    if stat.start_time != expected_start_time {
        // The number was recycled. This is the case the guard exists for.
        return Err(SignalError::Vanished);
    }
    if process::read_cmdline(pid).is_empty() && (pid == 2 || stat.ppid == 2) {
        return Err(SignalError::Refused(
            "this is a kernel thread, not a program — the kernel owns it".into(),
        ));
    }

    // Two refusals that exist because getting this wrong ends a session rather
    // than a program. A compositor grouped together with the apps it launched —
    // which is what happens when it runs as a systemd service, since everything
    // it starts inherits its cgroup — would otherwise be swept up in "close
    // this app" and take the desktop down with it.
    if is_compositor(&stat.comm) {
        return Err(SignalError::Refused(format!(
            "{} is the compositor running your desktop — closing it would end your \
             session and every window in it, so it is left alone",
            stat.comm
        )));
    }
    if is_ancestor_of_self(pid) {
        return Err(SignalError::Refused(format!(
            "{} started the session this monitor is running in — closing it would \
             take the whole session down, so it is left alone",
            stat.comm
        )));
    }

    // SAFETY: `kill` with a valid signal number is always safe to call; the
    // only question is whether it is permitted, which the return value answers.
    let result = unsafe { libc::kill(pid, signal.as_raw()) };
    if result == 0 {
        return Ok(());
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Err(SignalError::Vanished),
        Some(libc::EPERM) => Err(SignalError::NotPermitted),
        _ => Err(SignalError::Other(std::io::Error::last_os_error().to_string())),
    }
}

/// Whether a process is still present with the same identity.
pub fn still_running(pid: i32, expected_start_time: u64) -> bool {
    process::read_stat(pid).is_some_and(|s| s.start_time == expected_start_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compositor_is_never_signalled() {
        for name in ["niri", "Hyprland", "sway", "gnome-shell", "Xorg", "NIRI"] {
            assert!(is_compositor(name), "{name} must be protected");
        }
        // Xwayland can be restarted, so it stays killable.
        assert!(!is_compositor("Xwayland"));
        assert!(!is_compositor("firefox"));
    }

    /// The general guard: anything this process descends from would take us,
    /// and usually the session, with it.
    #[test]
    fn an_ancestor_of_this_process_is_never_signalled() {
        let parent = process::read_stat(std::process::id() as i32).unwrap().ppid;
        assert!(is_ancestor_of_self(parent), "our own parent must be recognised");

        let start = process::read_stat(parent).unwrap().start_time;
        assert!(
            matches!(send_signal(parent, start, Signal::Terminate), Err(SignalError::Refused(_))),
            "signalling our own parent must be refused"
        );
    }

    #[test]
    fn an_unrelated_process_is_not_mistaken_for_an_ancestor() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        assert!(!is_ancestor_of_self(pid), "our own child is not our ancestor");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The guard exercised against whatever compositor is actually running,
    /// rather than only against the name list. Skips where there is no
    /// graphical session, so it stays honest on a headless machine.
    #[test]
    fn the_running_compositor_is_refused_in_practice() {
        let running = crate::process::list_pids().into_iter().find_map(|pid| {
            let stat = crate::process::read_stat(pid)?;
            is_compositor(&stat.comm).then_some(stat)
        });
        let Some(stat) = running else {
            eprintln!("no compositor running; nothing to check");
            return;
        };
        assert!(
            matches!(
                send_signal(stat.pid, stat.start_time, Signal::Terminate),
                Err(SignalError::Refused(_))
            ),
            "{} (pid {}) was not refused",
            stat.comm,
            stat.pid
        );
    }

    #[test]
    fn init_is_never_signalled() {
        assert!(matches!(
            send_signal(1, 0, Signal::Terminate),
            Err(SignalError::Refused(_))
        ));
    }

    /// The happy path, against a process spawned for the purpose.
    #[test]
    fn a_signal_reaches_its_target() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn a victim");
        let pid = child.id() as i32;
        let start = process::read_stat(pid).expect("child is running").start_time;

        assert_eq!(send_signal(pid, start, Signal::Terminate), Ok(()));

        // Reap it so it does not linger as a zombie, which would still have a
        // matching start time and make `still_running` ambiguous.
        let status = child.wait().expect("child exits");
        assert!(!status.success(), "terminated by signal, so not a clean exit");
    }

    /// Signalling something that has already gone is not an error worth
    /// surfacing; it is the ordinary outcome of a race.
    #[test]
    fn signalling_a_dead_process_reports_it_vanished() {
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        let start = process::read_stat(pid).unwrap().start_time;
        let _ = send_signal(pid, start, Signal::Force);
        let _ = child.wait();
        assert_eq!(send_signal(pid, start, Signal::Terminate), Err(SignalError::Vanished));
    }

    /// The guard that stops a recycled pid from being killed by mistake.
    #[test]
    fn a_mismatched_start_time_sends_nothing() {
        let me = std::process::id() as i32;
        let real = process::read_stat(me).unwrap().start_time;
        assert_eq!(
            send_signal(me, real.wrapping_add(1), Signal::Force),
            Err(SignalError::Vanished),
            "a different start time means a different process"
        );
        // Still here, which is the point of the test.
        assert!(still_running(me, real));
    }
}
