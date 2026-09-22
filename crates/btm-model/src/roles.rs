//! Answers "why does this process exist?" in a sentence.
//!
//! Modern applications are not one process. A browser or an Electron app is a small
//! fleet: one supervisor, one per window, one for the GPU, one for sound, one for
//! the network. Every mainstream task manager shows that fleet as a dozen identical
//! rows and leaves the user to guess. The information needed to explain them is
//! already in the command line — it just has to be read.

use btm_probe::process;
use serde::{Deserialize, Serialize};

/// The job a process does, coarse enough to group and colour by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoleKind {
    /// The supervisor a user thinks of as "the app".
    Main,
    /// Draws and runs the content of a window or tab.
    Renderer,
    Gpu,
    Network,
    Audio,
    Storage,
    Media,
    Extension,
    /// A sandboxed helper with a specific job.
    Utility,
    /// A pre-warmed template used to spawn siblings cheaply.
    Zygote,
    /// Sits idle waiting to collect a crash report.
    CrashHandler,
    /// A background service rather than part of a visible app.
    Service,
    /// A kernel worker, not a program.
    Kernel,
}

impl RoleKind {
    /// Short label for grouping in the interface.
    pub fn group_label(self) -> &'static str {
        match self {
            RoleKind::Main => "Main process",
            RoleKind::Renderer => "Windows & tabs",
            RoleKind::Gpu => "Graphics",
            RoleKind::Network => "Network",
            RoleKind::Audio => "Audio",
            RoleKind::Storage => "Storage",
            RoleKind::Media => "Media",
            RoleKind::Extension => "Extensions",
            RoleKind::Utility => "Helpers",
            RoleKind::Zygote => "Process templates",
            RoleKind::CrashHandler => "Crash reporting",
            RoleKind::Service => "Service",
            RoleKind::Kernel => "Kernel",
        }
    }

    /// Whether a process of this kind is *expected* to look idle. Knowing this is
    /// what turns a confusing 0% into an unremarkable one.
    pub fn idle_by_design(self) -> bool {
        matches!(self, RoleKind::Zygote | RoleKind::CrashHandler | RoleKind::Main)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub kind: RoleKind,
    /// A few words, for the row.
    pub label: String,
    /// One plain sentence, for the detail panel.
    pub explanation: String,
}

impl Role {
    fn new(kind: RoleKind, label: &str, explanation: &str) -> Self {
        Role { kind, label: label.to_string(), explanation: explanation.to_string() }
    }
}

/// Works out what a process is for, from its command line and kernel name.
pub fn classify(comm: &str, cmdline: &[String]) -> Role {
    if cmdline.is_empty() {
        return Role::new(
            RoleKind::Kernel,
            "Kernel thread",
            "Part of the kernel itself, not a program you started. These handle things \
             like disk writeback and memory reclaim, and normally use very little CPU.",
        );
    }

    if let Some(ty) = process::flag_value(cmdline, "--type=") {
        return chromium_role(ty, cmdline);
    }
    if process::has_flag(cmdline, "-contentproc") || is_firefox_child(comm) {
        return firefox_role(comm, cmdline);
    }

    Role::new(
        RoleKind::Main,
        "Main process",
        "The process that starts and supervises the rest of the app. It usually shows \
         very little CPU itself, because the real work happens in the processes it \
         creates.",
    )
}

/// Chromium, Electron, and everything built on them: Chrome, Discord, VS Code, Slack.
fn chromium_role(ty: &str, cmdline: &[String]) -> Role {
    match ty {
        "renderer" => Role::new(
            RoleKind::Renderer,
            "Window or tab",
            "Runs the content of one window or tab. It is sandboxed, so if the page \
             inside it crashes the rest of the app survives. Heavy pages show up here.",
        ),
        "gpu-process" => Role::new(
            RoleKind::Gpu,
            "Graphics",
            "Talks to the graphics card for the whole app, so every window does not \
             need its own connection to the GPU. Expect activity whenever something \
             is animating or scrolling.",
        ),
        "zygote" => Role::new(
            RoleKind::Zygote,
            "Process template",
            "A pre-loaded template the app copies whenever it needs a new process. \
             Starting from a copy is much faster than starting from scratch. It is \
             meant to sit idle — near-zero CPU here is correct, not a glitch.",
        ),
        "broker" => Role::new(
            RoleKind::Utility,
            "Sandbox broker",
            "Grants the sandboxed processes the few system resources they are allowed \
             to touch. Almost always idle.",
        ),
        "crashpad-handler" => Role::new(
            RoleKind::CrashHandler,
            "Crash reporter",
            "Waits in the background to record a report if part of the app crashes. \
             It does nothing until that happens.",
        ),
        "utility" => utility_role(cmdline),
        other => Role::new(
            RoleKind::Utility,
            "Helper",
            &format!("A helper process the app describes as \"{other}\"."),
        ),
    }
}

/// Chromium utility processes announce their specific job in a second flag, which is
/// the difference between five identical "utility" rows and five explained ones.
fn utility_role(cmdline: &[String]) -> Role {
    let sub = process::flag_value(cmdline, "--utility-sub-type=").unwrap_or("");
    // Sub-types are Mojo interface names like `network.mojom.NetworkService`.
    let short = sub.rsplit('.').next().unwrap_or(sub);
    match short {
        "NetworkService" => Role::new(
            RoleKind::Network,
            "Network",
            "Handles all of this app's network traffic, kept separate from the code \
             that displays pages so a hostile page cannot reach the network directly.",
        ),
        "AudioService" => Role::new(
            RoleKind::Audio,
            "Audio",
            "Plays and records sound for this app. Expect steady light CPU use while \
             anything is playing.",
        ),
        "StorageService" => Role::new(
            RoleKind::Storage,
            "Storage",
            "Reads and writes this app's local data — settings, cookies and cached \
             files.",
        ),
        "VideoCaptureService" => Role::new(
            RoleKind::Media,
            "Camera",
            "Handles camera input for this app. Active only while a camera is in use.",
        ),
        "DataDecoderService" => Role::new(
            RoleKind::Media,
            "Data decoder",
            "Decodes untrusted data such as images and JSON in isolation, so malformed \
             content cannot damage the rest of the app.",
        ),
        "TracingService" => Role::new(
            RoleKind::Utility,
            "Diagnostics",
            "Collects internal performance traces for the app's own debugging. Idle in \
             normal use.",
        ),
        "" => Role::new(
            RoleKind::Utility,
            "Helper",
            "A sandboxed helper doing one specific job for the app.",
        ),
        other => Role::new(
            RoleKind::Utility,
            pretty_camel(other).as_str(),
            &format!(
                "A sandboxed helper the app uses for {}.",
                pretty_camel(other).to_lowercase()
            ),
        ),
    }
}

/// Firefox and its forks — Zen, LibreWolf, and Firefox web apps.
///
/// Firefox does not pass a `--type=` flag; it renames the process instead, and the
/// kernel truncates that name to 15 characters. Half the confusing entries in a
/// normal task manager are these truncated names.
fn firefox_role(comm: &str, cmdline: &[String]) -> Role {
    let child_id = process::flag_value(cmdline, "-childID").map(str::to_string);
    let suffix = child_id.map(|id| format!(" (child {id})")).unwrap_or_default();
    match comm {
        // "Isolated Web Content", truncated by the kernel.
        c if c.starts_with("Isolated Web") => Role::new(
            RoleKind::Renderer,
            "Tab",
            "Runs the pages for one site. Firefox gives each site its own process so \
             that one site cannot read another's data, which is why you see many of \
             these at once.",
        ),
        "Web Content" => Role::new(
            RoleKind::Renderer,
            "Tab",
            "Runs the content of one or more of your open tabs.",
        ),
        c if c.starts_with("Privileged Cont") => Role::new(
            RoleKind::Renderer,
            "Browser pages",
            "Runs the browser's own internal pages, such as settings and the new-tab \
             page, separately from ordinary websites.",
        ),
        c if c.starts_with("WebExtensions") => Role::new(
            RoleKind::Extension,
            "Extensions",
            "Runs your installed browser extensions. If the browser feels slow, this \
             is worth checking.",
        ),
        c if c.starts_with("RDD") => Role::new(
            RoleKind::Media,
            "Video decoder",
            "Decodes audio and video in isolation. Busy whenever you are watching \
             something.",
        ),
        c if c.starts_with("Socket Process") => Role::new(
            RoleKind::Network,
            "Network",
            "Handles the browser's network connections, separately from page content.",
        ),
        c if c.starts_with("GPU") || c.starts_with("Gpu") => Role::new(
            RoleKind::Gpu,
            "Graphics",
            "Talks to the graphics card for the browser.",
        ),
        c if c.starts_with("Utility") => Role::new(
            RoleKind::Utility,
            "Helper",
            "A sandboxed helper doing one specific job for the browser.",
        ),
        c if c.starts_with("forkserver") => Role::new(
            RoleKind::Zygote,
            "Process template",
            "Waits to spawn new browser processes on demand. Idle by design.",
        ),
        _ => Role::new(
            RoleKind::Utility,
            "Browser helper",
            &format!("A supporting process for the browser{suffix}."),
        ),
    }
}

fn is_firefox_child(comm: &str) -> bool {
    const NAMES: [&str; 8] = [
        "Isolated Web Co",
        "Web Content",
        "WebExtensions",
        "RDD Process",
        "Socket Process",
        "Privileged Cont",
        "Utility Process",
        "Isolated Servic",
    ];
    NAMES.iter().any(|n| comm.starts_with(n))
}

/// `VideoCaptureService` -> `Video capture`.
fn pretty_camel(s: &str) -> String {
    let trimmed = s.strip_suffix("Service").unwrap_or(s);
    let mut out = String::new();
    for (i, c) in trimmed.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn chromium_roles_are_read_from_a_flattened_argv() {
        // Electron children arrive as one blob; this is the real shape.
        let r = classify("vesktop", &flat("/usr/lib/vesktop/vesktop --type=renderer --enable-sandbox"));
        assert_eq!(r.kind, RoleKind::Renderer);

        let r = classify(
            "vesktop",
            &flat("/usr/lib/vesktop/vesktop --type=utility --utility-sub-type=network.mojom.NetworkService"),
        );
        assert_eq!(r.kind, RoleKind::Network);

        let r = classify("vesktop", &flat("/usr/lib/vesktop/vesktop --type=zygote"));
        assert_eq!(r.kind, RoleKind::Zygote);
        assert!(r.kind.idle_by_design());
    }

    /// The kernel truncates these names to 15 characters; matching must survive it.
    #[test]
    fn truncated_firefox_names_are_recognised() {
        let argv = vec!["/usr/lib/firefox/firefox".into(), "-contentproc".into()];
        assert_eq!(classify("Isolated Web Co", &argv).kind, RoleKind::Renderer);
        assert_eq!(classify("RDD Process", &argv).kind, RoleKind::Media);
        assert_eq!(classify("Socket Process", &argv).kind, RoleKind::Network);
        assert_eq!(classify("WebExtensions", &argv).kind, RoleKind::Extension);
    }

    #[test]
    fn a_process_with_no_argv_is_a_kernel_thread() {
        assert_eq!(classify("kworker/3:1", &[]).kind, RoleKind::Kernel);
    }

    #[test]
    fn a_plain_program_is_a_main_process() {
        let r = classify("bash", &["bash".to_string()]);
        assert_eq!(r.kind, RoleKind::Main);
        // The explanation has to pre-empt the "parent shows 0%" confusion.
        assert!(r.explanation.contains("processes it"));
    }

    #[test]
    fn unknown_utility_subtypes_get_a_readable_name() {
        assert_eq!(pretty_camel("VideoCaptureService"), "Video capture");
    }
}
