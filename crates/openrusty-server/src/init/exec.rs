//! Process glue for the `iptables-init` subcommand: logging, backend
//! probe, preflight self-checks, plan execution and the exit-code shell.
//! The rule surface itself is the pure [`super::build_rules`].

use super::{Backend, COMMENT, InitParams, PROBE_CHAIN, SUBCOMMAND};
use std::process::Command;
use tracing_subscriber::EnvFilter;

/// Entry point of the `iptables-init` subcommand; returns the process exit
/// code. All logging goes to stderr, so a `--dry-run` stdout stays a pure,
/// executable plan.
pub fn run_cli(argv: Vec<String>) -> i32 {
    init_logging();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", super::USAGE);
        return 0;
    }
    let params = match super::parse_args(&argv) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("openrusty {SUBCOMMAND}: {e}\nrun with --help for usage");
            return 2;
        }
    };
    match run(params) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "iptables-init failed");
            1
        }
    }
}

/// Runs the whole flow: plan -> (self-checks -> execute) or print.
pub fn run(params: InitParams) -> Result<(), String> {
    let rules = super::build_rules(params.clone());
    if params.dry_run {
        for rule in &rules {
            println!("{rule}");
        }
        return Ok(());
    }
    let bin = detect_backend(params.backend)?;
    check_conntrack()?;
    check_redirect(&bin)?;
    let (done, appended) = exec_plan(&bin, &rules)?;
    tracing::info!(
        commands = done,
        rules = appended,
        uid = params.proxy_uid,
        inbound = params.inbound_port,
        outbound = params.outbound_port,
        ignore_in = params.ignore_inbound_ports.len(),
        ignore_out = params.ignore_outbound_ports.len(),
        skip_subnets = params.skip_subnets.len(),
        backend = %bin,
        "iptables-init: parameter surface installed"
    );
    Ok(())
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .try_init();
}

/// `auto` probes the canonical name first, then the backend shims; a
/// forced backend is used as-is. Logs the probe outcome either way.
fn detect_backend(backend: Backend) -> Result<String, String> {
    let candidates: &[&str] = match backend {
        Backend::Iptables => &["iptables"],
        Backend::Auto => &["iptables", "iptables-nft", "iptables-legacy"],
    };
    for name in candidates {
        let ok = Command::new(name).arg("--version").status().map(|s| s.success()).unwrap_or(false);
        tracing::debug!(backend = name, available = ok, "probing iptables command");
        if ok {
            tracing::info!(backend = name, "iptables backend selected");
            return Ok((*name).to_string());
        }
    }
    Err("no usable iptables command (tried iptables, iptables-nft, iptables-legacy)".into())
}

/// REDIRECT is conntrack-based; without it the rules silently blackhole.
fn check_conntrack() -> Result<(), String> {
    if std::path::Path::new("/proc/net/nf_conntrack").exists()
        || std::path::Path::new("/proc/sys/net/netfilter").is_dir()
    {
        tracing::debug!("conntrack facility present");
        return Ok(());
    }
    let loaded = Command::new("modprobe").arg("nf_conntrack").output().map(|o| o.status.success()).unwrap_or(false);
    if loaded {
        tracing::info!("conntrack loaded via modprobe nf_conntrack");
        Ok(())
    } else {
        Err("conntrack unavailable: /proc/net/nf_conntrack missing and 'modprobe nf_conntrack' failed; \
             REDIRECT requires the conntrack facility"
            .into())
    }
}

/// Proves the kernel actually accepts a REDIRECT rule (the nat/redirect
/// target can be compiled out) by writing one into a scratch chain and
/// taking it down again. Always tears down, even on failure.
fn check_redirect(bin: &str) -> Result<(), String> {
    let _ = exec(bin, &["-t", "nat", "-N", PROBE_CHAIN]); // tolerated if it exists
    let probe = format!("-t nat -A {PROBE_CHAIN} -p tcp -m comment --comment {COMMENT} -j REDIRECT --to-ports 1");
    let result = exec(bin, &probe.split(' ').collect::<Vec<&str>>()).map_err(|_| {
        "REDIRECT target unavailable: kernel rejected a REDIRECT rule (nat/redirect target missing?)".to_string()
    });
    let _ = exec(bin, &["-t", "nat", "-F", PROBE_CHAIN]);
    let _ = exec(bin, &["-t", "nat", "-X", PROBE_CHAIN]);
    result
}

/// Executes the plan. Semantics per line (lines are `iptables -t nat <op>
/// ...`, so the op is argv word 3): `-N` tolerates "already exists";
/// `-C` is a guard whose success skips the adjacent `-I`; everything else
/// must succeed or the run aborts, reporting how far it got.
fn exec_plan(bin: &str, rules: &[String]) -> Result<(usize, usize), String> {
    let mut executed = 0;
    let mut appended = 0;
    let mut skip_insert = false;
    for (idx, rule) in rules.iter().enumerate() {
        let mut words: Vec<&str> = rule.split(' ').collect();
        words[0] = bin;
        let op = words.get(3).copied().unwrap_or_default();
        if skip_insert && op == "-I" {
            skip_insert = false;
            executed += 1;
            tracing::debug!(rule, "hook already present; skipping insert");
            continue;
        }
        skip_insert = false;
        let outcome = exec(bin, &words[1..]);
        match (op, outcome) {
            ("-N", Err(e)) => {
                executed += 1;
                tracing::debug!(rule, error = %e, "chain probably exists; continuing");
            }
            ("-C", Ok(())) => {
                executed += 1;
                skip_insert = true;
            }
            ("-C", Err(_)) => executed += 1, // hook absent: the next -I installs it
            (_, Ok(())) => {
                executed += 1;
                if op == "-A" {
                    appended += 1;
                }
                tracing::debug!(rule, "applied");
            }
            (_, Err(e)) => {
                return Err(format!(
                    "command {}/{} failed: {e}; {executed} command(s) applied before abort: {rule}",
                    idx + 1,
                    rules.len()
                ))
            }
        }
    }
    Ok((executed, appended))
}

fn exec(bin: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin).args(args).output().map_err(|e| format!("spawn {bin}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: String = stderr.trim().chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect();
    Err(format!("{bin} exited with {}: {tail}", out.status))
}
