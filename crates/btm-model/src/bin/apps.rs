//! The model, rendered as text. This is the process view the project exists to
//! build, without the graphics.

use btm_model::{App, AppKind, ProcNode, SystemModel};
use btm_probe::desktop::DesktopDb;
use btm_probe::{Sampler, conf, sampler::SamplerConfig, wm};
use std::thread;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let num = |name: &str, default: f64| -> f64 {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let interval = num("--interval", 2.0);
    let limit = num("--top", 10.0) as usize;
    let expand = args.iter().any(|a| a == "--tree");
    let only = args
        .iter()
        .position(|a| a == "--app")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let db = DesktopDb::load();
    let mut sampler = Sampler::new(SamplerConfig {
        pss_interval: Duration::from_millis(0),
        pss_top_n: 512,
        ..Default::default()
    });

    sampler.sample()?;
    thread::sleep(Duration::from_secs_f64(interval));
    let sample = sampler.sample()?;
    let model = SystemModel::build(&sample, &wm::windows(), &db);

    header(&model, &db);

    let shown: Vec<&App> = match &only {
        Some(pat) => {
            let needle = pat.to_lowercase();
            model
                .apps
                .iter()
                .filter(|a| {
                    a.name.to_lowercase().contains(&needle) || a.id.to_lowercase().contains(&needle)
                })
                .collect()
        }
        None => model.apps.iter().filter(|a| a.kind != AppKind::Kernel).take(limit).collect(),
    };

    for app in shown {
        print_app(app, expand || only.is_some());
    }

    if only.is_none() {
        footer(&model);
    }
    Ok(())
}

fn header(m: &SystemModel, db: &DesktopDb) {
    println!();
    println!(
        "  cpu {:>5.1}%  of {} threads      memory {} of {}      {} apps, {} processes",
        m.cpu_busy.unwrap_or(0.0) * 100.0,
        conf::cpu_count(),
        gb(m.mem.used()),
        gb(m.mem.total),
        m.apps.len(),
        m.apps.iter().map(|a| a.process_count).sum::<usize>(),
    );
    // Utilisation alone cannot distinguish a machine working hard from one in
    // trouble. Pressure can.
    if let Some(p) = m.pressure.cpu {
        let verdict = if p.some.avg10 < 1.0 {
            "nothing is waiting on the cpu"
        } else if p.some.avg10 < 20.0 {
            "some waiting for cpu time"
        } else {
            "processes are contending for the cpu"
        };
        println!("  pressure  {:.1}% — {}", p.some.avg10, verdict);
    }
    println!("  matched against {} desktop entries", db.len());
    println!();
}

fn print_app(app: &App, expand: bool) {
    let est = if app.totals.is_approximate() { "~" } else { "" };
    println!(
        "  {:<26} {:>6.1}%  {}{:>9}   {} process{}{}",
        truncate(&app.name, 26),
        app.totals.cpu_percent_of_machine(),
        est,
        mb(app.totals.mem_pss),
        app.process_count,
        if app.process_count == 1 { "" } else { "es" },
        confidence(app),
    );

    if let Some(title) = app.windows.first() {
        let extra = app.windows.len().saturating_sub(1);
        let more = if extra > 0 { format!(" (+{extra} more)") } else { String::new() };
        println!("      window   {}{}", truncate(title, 56), more);
    }

    // The comparison other tools get wrong.
    let over = app.totals.rss_overcount();
    if app.process_count > 1 && over > 100 * 1024 * 1024 {
        println!(
            "      memory   {} actual — tools that sum RSS would say {} ({:.1}x)",
            mb(app.totals.mem_pss),
            mb(app.totals.mem_rss),
            app.totals.mem_rss as f64 / app.totals.mem_pss.max(1) as f64
        );
    }

    if expand {
        let parts: Vec<String> = app
            .composition()
            .iter()
            .map(|(kind, n)| format!("{n} × {}", kind.group_label().to_lowercase()))
            .collect();
        if app.process_count > 1 {
            println!("      made of  {}", parts.join(", "));
        }
        for root in &app.roots {
            print_node(root, 6);
        }
    }
    println!();
}

fn print_node(node: &ProcNode, indent: usize) {
    let pad = " ".repeat(indent);
    let subtree = node.subtree_usage.cpu_percent_of_machine();
    let own = node.self_usage.cpu_percent_of_machine();

    // Both numbers, always: the subtree figure is the honest headline, and showing
    // the process's own alongside it is what makes an idle-looking parent make
    // sense instead of looking like a bug.
    let cpu = if node.children.is_empty() {
        format!("{own:>5.1}%")
    } else {
        format!("{subtree:>5.1}% ({own:.1}% own)")
    };

    println!(
        "{pad}{:<7} {:<18} {:<16} {}",
        node.pid,
        truncate(node.display_name(), 18),
        cpu,
        truncate(&node.role.label, 16),
    );

    if node.work_is_in_children() {
        println!(
            "{pad}        ↳ idle itself; the work is in the {} process{} below it",
            node.descendant_count(),
            if node.descendant_count() == 1 { "" } else { "es" }
        );
    }
    for c in &node.children {
        print_node(c, indent + 2);
    }
}

fn footer(m: &SystemModel) {
    let kernel: usize =
        m.apps.iter().filter(|a| a.kind == AppKind::Kernel).map(|a| a.process_count).sum();
    let bg = m.apps.iter().filter(|a| a.kind == AppKind::Background).count();
    let sys = m.apps.iter().filter(|a| a.kind == AppKind::System).count();
    println!("  not shown: {kernel} kernel threads, {bg} background, {sys} system services");
    println!("  --tree to expand processes, --app <name> for one app");
    println!();
}

/// Marks groupings that rest on a weak signal, rather than showing every guess with
/// the same confidence as a fact.
fn confidence(app: &App) -> &'static str {
    if app.identified_by.is_certain() { "" } else { "  ·" }
}

fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1024.0 / 1024.0)
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}
