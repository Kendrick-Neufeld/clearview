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
