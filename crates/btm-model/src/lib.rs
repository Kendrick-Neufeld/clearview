//! Turns a flat list of processes into a handful of applications a person
//! recognises, each one able to explain itself.
//!
//! The three problems this layer exists to solve, all of them visible in any
//! mainstream task manager:
//!
//! 1. One application appears as many unrelated rows sharing a truncated name.
//! 2. A parent reads 0% while its child burns a core, because the headline figure
//!    is per-process rather than per-subtree.
//! 3. Nothing says why a process exists, even though its command line says so.

pub mod cgroup;
pub mod identity;
pub mod roles;
pub mod usage;

use btm_probe::process::ProcKey;
use btm_probe::wm::Window;
use btm_probe::{ProcSample, Sample, desktop::DesktopDb};
use identity::{IdentitySource, Resolver};
use roles::{Role, RoleKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use usage::Usage;

/// One process, with its descendants nested beneath it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcNode {
    pub key: ProcKey,
    pub pid: i32,
    pub ppid: i32,
    /// The kernel's name for it, truncated to 15 characters.
    pub comm: String,
    pub role: Role,
    /// The title of the window this process draws, when it draws one. This is what
    /// turns an anonymous renderer into "Discord — Lobby".
    pub window_title: Option<String>,
    pub state: char,
    pub threads: i64,
    /// What this process alone is doing.
    pub self_usage: Usage,
    /// What this process and everything below it are doing. This is the headline
    /// number, and the reason a launcher no longer looks mysteriously idle.
    pub subtree_usage: Usage,
    pub children: Vec<ProcNode>,
    pub cmdline: Vec<String>,
}

impl ProcNode {
    /// The most useful name available: what the window says if it draws one,
    /// otherwise the program's own name. The role is shown alongside rather than
    /// in place of it, so every helper is not just called "Main process".
    pub fn display_name(&self) -> &str {
        self.window_title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.comm)
    }

    /// True when this process looks idle but is responsible for real work happening
    /// below it. Flagging this is the direct answer to "the parent says 0% but its
    /// child says 50%, which makes no sense".
    pub fn work_is_in_children(&self) -> bool {
        let below = self.subtree_usage.cpu_cores - self.self_usage.cpu_cores;
        self.self_usage.cpu_cores < 0.02 && below > 0.05
    }

    pub fn descendant_count(&self) -> usize {
        self.children.iter().map(|c| 1 + c.descendant_count()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppKind {
    /// Something with a window; what a user means by "an app".
    Gui,
    /// Runs for the user, without a window.
    Background,
    /// A system-wide service, running whether anyone is logged in or not.
    System,
    /// Kernel workers, grouped together and kept away from real applications.
    Kernel,
}

/// One application: everything belonging to it, however many processes that is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    /// Stable grouping key.
    pub id: String,
    /// What to call it on screen.
    pub name: String,
    pub icon: Option<String>,
    pub kind: AppKind,
    /// How the grouping was determined, so the interface can qualify weak matches
    /// instead of presenting every guess with equal confidence.
    pub identified_by: IdentitySource,
    /// Titles of the windows this app currently has open.
    pub windows: Vec<String>,
    /// Process trees. Usually one; more when an app's processes were re-parented.
    pub roots: Vec<ProcNode>,
    /// The app's total, summed over its processes. Memory is PSS, so this is a
    /// real figure rather than the inflated sum of RSS other tools report.
    pub totals: Usage,
    pub process_count: usize,
    /// Why closing this would be a bad idea, when it would be. Present so the
    /// interface can warn in the app's own terms instead of asking the person
    /// to recognise a process name.
    pub caution: Option<String>,
}

impl App {
    /// A short account of how this app is put together, e.g.
    /// "8 processes: 1 main, 1 window or tab, 3 process templates".
    pub fn composition(&self) -> Vec<(RoleKind, usize)> {
        let mut counts: HashMap<RoleKind, usize> = HashMap::new();
        for root in &self.roots {
            count_roles(root, &mut counts);
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        v
    }
}

fn count_roles(node: &ProcNode, counts: &mut HashMap<RoleKind, usize>) {
    *counts.entry(node.role.kind).or_default() += 1;
    for c in &node.children {
        count_roles(c, counts);
    }
}

/// Everything the interface needs for one refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemModel {
    pub apps: Vec<App>,
    /// Machine-wide CPU busy fraction, 0.0 to 1.0.
    pub cpu_busy: Option<f64>,
    pub per_cpu_busy: Vec<f64>,
    pub mem: btm_probe::system::MemInfo,
    pub pressure: btm_probe::system::SystemPressure,
    pub interval_secs: Option<f64>,

    pub thermals: btm_probe::sensors::Thermals,
    pub power_w: Option<f32>,
    pub core_mhz: Vec<u32>,
    /// Which physical core each logical CPU sits on, so a hyper-thread can be
    /// shown with the temperature it actually shares.
    pub core_of_cpu: Vec<usize>,
    pub gpus: Vec<btm_probe::gpu::GpuInfo>,
    pub interfaces: Vec<btm_probe::sampler::InterfaceRate>,
    pub disks: Vec<btm_probe::sampler::DiskRate>,
}

impl SystemModel {
    /// Builds the model. `windows` may be empty under a compositor we cannot query;
    /// everything still works, with less helpful names.
    pub fn build(sample: &Sample, windows: &[Window], db: &DesktopDb) -> Self {
        let resolver = Resolver::new(&sample.procs, windows, db);

        let mut groups: HashMap<String, (IdentitySource, Vec<&ProcSample>)> = HashMap::new();
        for p in &sample.procs {
            let id = resolver.resolve(p);
            let slot = groups.entry(id.app_id).or_insert((id.source, Vec::new()));
            // Keep the strongest source that contributed to the group, so an app
            // is not labelled a guess just because one late child was inherited.
            if (id.source as u8) < (slot.0 as u8) {
                slot.0 = id.source;
            }
            slot.1.push(p);
        }

        let mut apps: Vec<App> =
            groups.into_iter().map(|(id, (src, procs))| build_app(id, src, &procs, &resolver, db)).collect();

        // Busiest first, memory breaking ties, so the answer to "what is using my
        // machine" is always at the top.
        apps.sort_by(|a, b| {
            b.totals
                .cpu_cores
                .partial_cmp(&a.totals.cpu_cores)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.totals.mem_pss.cmp(&a.totals.mem_pss))
        });

        SystemModel {
            apps,
            cpu_busy: sample.cpu_busy,
            per_cpu_busy: sample.per_cpu_busy.clone(),
            mem: sample.mem,
            pressure: sample.pressure,
            interval_secs: sample.interval_secs,
            thermals: sample.thermals.clone(),
            power_w: sample.power_w,
            core_mhz: sample.core_mhz.clone(),
            core_of_cpu: sample.core_of_cpu.clone(),
            gpus: sample.gpus.clone(),
            interfaces: sample.interfaces.clone(),
            disks: sample.disks.clone(),
        }
    }

    /// Apps with windows, which is what most people want to look at first.
    pub fn gui_apps(&self) -> impl Iterator<Item = &App> {
        self.apps.iter().filter(|a| a.kind == AppKind::Gui)
    }
}

fn build_app(
    id: String,
    source: IdentitySource,
    procs: &[&ProcSample],
    resolver: &Resolver,
    db: &DesktopDb,
) -> App {
    let members: HashSet<i32> = procs.iter().map(|p| p.key.pid).collect();
    let by_pid: HashMap<i32, &ProcSample> = procs.iter().map(|p| (p.key.pid, *p)).collect();

    // Children of this app that are themselves in this app. Processes re-parented
    // out of the group are not pulled back in, which keeps the totals from being
    // counted twice.
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut roots: Vec<i32> = Vec::new();
    for p in procs {
        if members.contains(&p.ppid) && p.ppid != p.key.pid {
            children.entry(p.ppid).or_default().push(p.key.pid);
        } else {
            roots.push(p.key.pid);
        }
    }
    roots.sort_unstable();

    let nodes: Vec<ProcNode> = roots
        .iter()
        .map(|pid| build_node(*pid, &by_pid, &children, resolver, 0))
        .collect();

    // The app total sums each process once. Summing subtree totals instead would
    // count every nested process as many times as it is deep.
    let totals: Usage = procs.iter().map(|p| Usage::of(p)).sum();

    let window_titles: Vec<String> = procs
        .iter()
        .flat_map(|p| resolver.windows_for(p.key.pid))
        .map(|w| w.title.clone())
        .filter(|t| !t.is_empty())
        .collect();

    let entry = db
        .by_id(&id)
        .or_else(|| db.by_wm_class(&id))
        .or_else(|| db.by_exec_name(&id))
        .or_else(|| {
            // Fall back to the name of whichever process is the app's root.
            let root = roots.first().and_then(|p| by_pid.get(p))?;
            db.by_exec_name(&identity::executable_name(root))
        });

    let kind = if source == IdentitySource::Kernel {
        AppKind::Kernel
    } else if !window_titles.is_empty() {
        AppKind::Gui
    } else if matches!(source, IdentitySource::SystemdService)
        && procs.iter().any(|p| p.cgroup.as_deref().is_some_and(|c| c.starts_with("/system.slice")))
    {
        AppKind::System
    } else {
        AppKind::Background
    };

    let caution = caution_for(&id, kind, procs);

    App {
        name: match (&entry, kind) {
            (Some(e), _) => e.name.clone(),
            (None, AppKind::Kernel) => "Kernel".to_string(),
            (None, _) => id.clone(),
        },
        icon: entry.and_then(|e| e.icon.clone()),
        id,
        kind,
        identified_by: source,
        windows: window_titles,
        process_count: procs.len(),
        totals,
        roots: nodes,
        caution,
    }
}

/// Whether closing an app would take something important with it.
///
/// The judgement lives here rather than in the interface because it is domain
/// knowledge: which names belong to a desktop session, and what a cgroup slice
/// implies. A warning names the consequence — "this would end your desktop
/// session" — rather than the process, because the consequence is what someone
/// is actually deciding about.
fn caution_for(id: &str, kind: AppKind, procs: &[&ProcSample]) -> Option<String> {
    // Compositors and display servers. Ending one closes every window with it.
    const SESSION: [&str; 9] = [
        "Hyprland", "sway", "river", "niri", "gnome-shell", "plasmashell", "Xorg", "Xwayland",
        "weston",
    ];
    // Infrastructure that other programs depend on while they run.
    const PLUMBING: [&str; 6] =
        ["pipewire", "pipewire-pulse", "wireplumber", "dbus-broker", "systemd", "gnome-keyring-daemon"];

    let names: Vec<&str> = procs.iter().map(|p| p.comm.as_str()).collect();
    let matches = |list: &[&str]| {
        list.iter().any(|n| id.eq_ignore_ascii_case(n) || names.iter().any(|c| c == n))
    };

    match kind {
        AppKind::Kernel => Some(
            "These belong to the kernel itself. They cannot be closed, and nothing here will try."
                .into(),
        ),
        _ if matches(&SESSION) => Some(
            "This is your desktop itself. Closing it would end your session and shut every              window you have open."
                .into(),
        ),
        _ if matches(&PLUMBING) => Some(
            "Other programs rely on this while they are running. Closing it is likely to break              sound, settings or logins until you sign in again."
                .into(),
        ),
        AppKind::System => Some(
            "This is a system service. It belongs to the operating system rather than to you,              and closing it usually needs administrator rights."
                .into(),
        ),
        _ => None,
    }
}

fn build_node(
    pid: i32,
    by_pid: &HashMap<i32, &ProcSample>,
    children: &HashMap<i32, Vec<i32>>,
    resolver: &Resolver,
    depth: u32,
) -> ProcNode {
    let p = by_pid[&pid];
    // A depth cap keeps a pathological or racing `/proc` from recursing forever.
    let kids: Vec<ProcNode> = if depth < 64 {
        let mut ids = children.get(&pid).cloned().unwrap_or_default();
        ids.sort_unstable();
        ids.iter().map(|c| build_node(*c, by_pid, children, resolver, depth + 1)).collect()
    } else {
        Vec::new()
    };

    let self_usage = Usage::of(p);
    let mut subtree_usage = self_usage;
    for k in &kids {
        subtree_usage += k.subtree_usage;
    }

    ProcNode {
        key: p.key,
        pid,
        ppid: p.ppid,
        comm: p.comm.clone(),
        role: roles::classify(&p.comm, &p.cmdline),
        window_title: resolver.windows_for(pid).first().map(|w| w.title.clone()),
        state: p.state,
        threads: p.num_threads,
        self_usage,
        subtree_usage,
        children: kids,
        cmdline: p.cmdline.clone(),
    }
}
