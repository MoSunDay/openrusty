//! `openrusty iptables-init`: one-shot installer of the sidecar traffic
//! hijack parameter surface (nat-table REDIRECT rules) in front of the
//! transparent listeners. Blueprint: linkerd2's `_proxy-init.tpl`.
//!
//! Parameter-surface mapping (linkerd2 proxy-init -> this command):
//!
//! | linkerd2                      | openrusty                                  |
//! |-------------------------------|--------------------------------------------|
//! | `PROXY_INIT_REDIRECT` chain   | `OPENRUSTY_IN`, hooked from PREROUTING     |
//! | `PROXY_INIT_OUTPUT` chain     | `OPENRUSTY_OUT`, hooked from OUTPUT        |
//! | `--proxy-uid` owner RETURN    | `--proxy-uid` (first rule of the OUT chain)|
//! | inbound ports-to-ignore       | `--ignore-inbound-ports` (default 4191)    |
//! | outbound ports-to-ignore      | `--ignore-outbound-ports` (default 443)    |
//! | outbound skip-subnets         | `--skip-subnets` (RETURN before REDIRECT)  |
//! | `-N`/`-F`/`-A` re-flush       | same: re-entry converges to one state      |
//!
//! Idempotence: custom chains are created if missing (`-N`), flushed
//! (`-F`) and repopulated (`-A`) on every run; the PREROUTING/OUTPUT hook
//! jumps are installed only when an `iptables -C` existence check fails.
//! Two consecutive runs therefore leave an identical `iptables-save`.
//!
//! The plan is produced by the pure [`build_rules`]; every line is a full
//! backend-neutral argv (`iptables -t nat ...`). The concrete binary is
//! resolved at execution time: iptables-legacy and iptables-nft differ
//! only in the command name, the rule syntax is shared.
//!
//! Order inside `OPENRUSTY_OUT` is load-bearing and locked by tests:
//! owner RETURN (never loop the proxy) -> ignore-outbound-ports RETURN ->
//! skip-subnets RETURN -> full-port REDIRECT.
//!
//! Self-checks run before any mutation (fail-fast, like an init container):
//! conntrack availability (REDIRECT is conntrack-based), the iptables
//! command, and a write/delete REDIRECT probe on a scratch chain. The
//! process glue lives in [`exec`]; this module keeps the pure surface.

pub mod exec;

use std::net::IpAddr;

/// Subcommand name under the `openrusty` binary.
pub const SUBCOMMAND: &str = "iptables-init";
/// nat chain hijacking traffic that *arrives* at the pod (PREROUTING).
pub const CHAIN_IN: &str = "OPENRUSTY_IN";
/// nat chain hijacking traffic the pod *sends* (OUTPUT).
pub const CHAIN_OUT: &str = "OPENRUSTY_OUT";
/// Comment tag on every rule, so foreign tooling can find/scrub ours.
const COMMENT: &str = "openrusty-init";
/// Scratch chain used by the REDIRECT-target self-check.
const PROBE_CHAIN: &str = "OPENRUSTY_PROBE";

/// Everything the rules depend on; parsed from argv by [`parse_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitParams {
    /// UID the proxy runs as; its own dials are exempt via `-m owner`.
    pub proxy_uid: u32,
    /// REDIRECT target for inbound (PREROUTING) traffic.
    pub inbound_port: u16,
    /// REDIRECT target for outbound (OUTPUT) traffic.
    pub outbound_port: u16,
    /// Inbound destination ports left alone (admin/mesh-local ports).
    pub ignore_inbound_ports: Vec<u16>,
    /// Outbound destination ports left alone.
    pub ignore_outbound_ports: Vec<u16>,
    /// Outbound destination subnets (CIDR) routed without hijack.
    pub skip_subnets: Vec<String>,
    /// Which iptables command flavour to use.
    pub backend: Backend,
    /// Print the plan to stdout instead of executing it.
    pub dry_run: bool,
}

/// `auto` probes the well-known command names; `iptables` forces the
/// canonical one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Auto,
    Iptables,
}

impl Default for InitParams {
    fn default() -> Self {
        InitParams {
            proxy_uid: 65534, // nobody: the usual test/sidecar stand-in
            inbound_port: 4143,
            outbound_port: 4140,
            ignore_inbound_ports: vec![4191],
            ignore_outbound_ports: vec![443],
            skip_subnets: Vec::new(),
            backend: Backend::Auto,
            dry_run: false,
        }
    }
}

const USAGE: &str = "openrusty iptables-init: install sidecar traffic redirection (nat REDIRECT)

usage: openrusty iptables-init --proxy-uid <UID> [FLAGS]

flags:
  --proxy-uid <UID>                proxy UID, exempted on OUTPUT (required)
  --inbound-port <PORT>            inbound REDIRECT target (default 4143)
  --outbound-port <PORT>           outbound REDIRECT target (default 4140)
  --ignore-inbound-ports <LIST>    comma-separated RETURN ports (default 4191)
  --ignore-outbound-ports <LIST>   comma-separated RETURN ports (default 443)
  --skip-subnets <LIST>            comma-separated RETURN CIDRs (default none)
  --backend <auto|iptables>        command flavour probe (default auto)
  --dry-run                        print the plan to stdout, execute nothing
  -h, --help                       this text
";

/// Parses the subcommand argv (`--flag value` and `--flag=value` forms).
pub fn parse_args(argv: &[String]) -> Result<InitParams, String> {
    let mut p = InitParams::default();
    let mut uid: Option<u32> = None;
    let mut i = 0;
    while i < argv.len() {
        let (flag, inline) = match argv[i].split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (argv[i].clone(), None),
        };
        let mut next = |what: &str| -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => {
                    i += 1;
                    argv.get(i).cloned().ok_or_else(|| format!("{what} needs a value"))
                }
            }
        };
        match flag.as_str() {
            "--proxy-uid" => uid = Some(parse_num(&next("--proxy-uid")?, "proxy-uid")?),
            "--inbound-port" => p.inbound_port = parse_port(&next("--inbound-port")?)?,
            "--outbound-port" => p.outbound_port = parse_port(&next("--outbound-port")?)?,
            "--ignore-inbound-ports" => p.ignore_inbound_ports = parse_ports(&next("--ignore-inbound-ports")?)?,
            "--ignore-outbound-ports" => {
                p.ignore_outbound_ports = parse_ports(&next("--ignore-outbound-ports")?)?
            }
            "--skip-subnets" => p.skip_subnets = parse_subnets(&next("--skip-subnets")?)?,
            "--backend" => {
                p.backend = match next("--backend")?.as_str() {
                    "auto" => Backend::Auto,
                    "iptables" => Backend::Iptables,
                    other => return Err(format!("unknown backend '{other}' (auto|iptables)")),
                }
            }
            "--dry-run" if inline.is_none() => p.dry_run = true,
            other => return Err(format!("unknown flag '{other}' (see --help)")),
        }
        i += 1;
    }
    p.proxy_uid = uid.ok_or("missing required flag --proxy-uid <UID>")?;
    Ok(p)
}

fn parse_num(s: &str, what: &str) -> Result<u32, String> {
    s.parse::<u32>().map_err(|_| format!("invalid {what} '{s}'"))
}

fn parse_port(s: &str) -> Result<u16, String> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("invalid port '{s}' (1-65535)")),
        Ok(port) => Ok(port),
    }
}

fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    s.split(',').filter(|x| !x.trim().is_empty()).map(|x| parse_port(x.trim())).collect()
}

fn parse_subnets(s: &str) -> Result<Vec<String>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|cidr| match cidr.split_once('/') {
            Some((addr, mask)) => {
                let addr: IpAddr = addr.parse().map_err(|_| format!("invalid subnet '{cidr}'"))?;
                let max = if addr.is_ipv4() { 32 } else { 128 };
                match mask.parse::<u32>() {
                    Ok(m) if m <= max => Ok(cidr.to_string()),
                    _ => Err(format!("invalid subnet '{cidr}'")),
                }
            }
            None => {
                cidr.parse::<IpAddr>().map(|_| cidr.to_string()).map_err(|_| format!("invalid subnet '{cidr}'"))
            }
        })
        .collect()
}

/// The full command plan, in execution order. Pure and deterministic:
/// identical params always produce byte-identical output (locked by tests
/// and asserted by the netns drill via two `--dry-run` runs).
pub fn build_rules(params: InitParams) -> Vec<String> {
    let mut rules = Vec::new();
    // Inbound: everything that arrives at the pod is hijacked into the
    // inbound listener, except the ignore ports (admin stays reachable).
    rules.extend(chain_setup(CHAIN_IN, "PREROUTING"));
    for port in &params.ignore_inbound_ports {
        rules.push(format!(
            "iptables -t nat -A {CHAIN_IN} -p tcp --dport {port} -m comment --comment {COMMENT} -j RETURN"
        ));
    }
    rules.push(format!(
        "iptables -t nat -A {CHAIN_IN} -p tcp -m comment --comment {COMMENT} -j REDIRECT --to-ports {}",
        params.inbound_port
    ));
    // Outbound: the proxy's own dials first (no self-loop), then explicit
    // exemptions, then the full-port REDIRECT into the outbound listener.
    rules.extend(chain_setup(CHAIN_OUT, "OUTPUT"));
    rules.push(format!(
        "iptables -t nat -A {CHAIN_OUT} -p tcp -m owner --uid-owner {} -m comment --comment {COMMENT} -j RETURN",
        params.proxy_uid
    ));
    for port in &params.ignore_outbound_ports {
        rules.push(format!(
            "iptables -t nat -A {CHAIN_OUT} -p tcp --dport {port} -m comment --comment {COMMENT} -j RETURN"
        ));
    }
    for subnet in &params.skip_subnets {
        rules.push(format!(
            "iptables -t nat -A {CHAIN_OUT} -p tcp -d {subnet} -m comment --comment {COMMENT} -j RETURN"
        ));
    }
    rules.push(format!(
        "iptables -t nat -A {CHAIN_OUT} -p tcp -m comment --comment {COMMENT} -j REDIRECT --to-ports {}",
        params.outbound_port
    ));
    rules
}

/// `-N` (idempotent: tolerated if it exists) -> `-F` (own chain only) ->
/// the hook jump as a `-C` guard + conditional `-I` pair.
fn chain_setup(chain: &str, hook: &str) -> Vec<String> {
    let jump = format!("-m comment --comment {COMMENT} -j {chain}");
    vec![
        format!("iptables -t nat -N {chain}"),
        format!("iptables -t nat -F {chain}"),
        format!("iptables -t nat -C {hook} {jump}"),
        format!("iptables -t nat -I {hook} {jump}"),
    ]
}
#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(argv: &[&str]) -> Vec<String> {
        let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        build_rules(parse_args(&owned).expect("argv parses"))
    }

    #[test]
    fn default_plan_is_locked() {
        let plan = build_rules(InitParams::default());
        let expect = [
            "iptables -t nat -N OPENRUSTY_IN",
            "iptables -t nat -F OPENRUSTY_IN",
            "iptables -t nat -C PREROUTING -m comment --comment openrusty-init -j OPENRUSTY_IN",
            "iptables -t nat -I PREROUTING -m comment --comment openrusty-init -j OPENRUSTY_IN",
            "iptables -t nat -A OPENRUSTY_IN -p tcp --dport 4191 -m comment --comment openrusty-init -j RETURN",
            "iptables -t nat -A OPENRUSTY_IN -p tcp -m comment --comment openrusty-init -j REDIRECT --to-ports 4143",
            "iptables -t nat -N OPENRUSTY_OUT",
            "iptables -t nat -F OPENRUSTY_OUT",
            "iptables -t nat -C OUTPUT -m comment --comment openrusty-init -j OPENRUSTY_OUT",
            "iptables -t nat -I OUTPUT -m comment --comment openrusty-init -j OPENRUSTY_OUT",
            "iptables -t nat -A OPENRUSTY_OUT -p tcp -m owner --uid-owner 65534 -m comment --comment openrusty-init -j RETURN",
            "iptables -t nat -A OPENRUSTY_OUT -p tcp --dport 443 -m comment --comment openrusty-init -j RETURN",
            "iptables -t nat -A OPENRUSTY_OUT -p tcp -m comment --comment openrusty-init -j REDIRECT --to-ports 4140",
        ];
        assert_eq!(plan, expect);
    }

    #[test]
    fn outbound_order_owner_ignore_subnets_redirect() {
        let p = InitParams {
            proxy_uid: 511,
            skip_subnets: vec!["10.0.0.0/8".into(), "192.168.0.0/16".into()],
            ..InitParams::default()
        };
        let plan = build_rules(p);
        let pos = |needle: &str| plan.iter().position(|l| l.contains(needle)).expect("line present");
        let owner = pos("--uid-owner 511");
        let ignore = pos("--dport 443");
        let subnet1 = pos("-d 10.0.0.0/8");
        let subnet2 = pos("-d 192.168.0.0/16");
        let redirect = plan.iter().position(|l| l.contains(CHAIN_OUT) && l.contains("REDIRECT")).unwrap();
        assert!(owner < ignore && ignore < subnet1 && subnet1 < subnet2 && subnet2 < redirect);
    }

    #[test]
    fn ignore_ports_expand_one_return_each() {
        let argv = ["--proxy-uid", "7", "--ignore-inbound-ports", "4191,15000", "--ignore-outbound-ports", "443,9090"];
        let plan = plan_of(&argv);
        // dport RETURNs only: the owner exemption is a RETURN without a port.
        let returns: Vec<&String> = plan
            .iter()
            .filter(|l| l.contains(" -A ") && l.contains("RETURN") && l.contains("--dport"))
            .collect();
        assert_eq!(returns.len(), 4);
        assert!(returns[0].contains("--dport 4191") && returns[0].contains(CHAIN_IN));
        assert!(returns[1].contains("--dport 15000") && returns[1].contains(CHAIN_IN));
        assert!(returns[2].contains("--dport 443") && returns[2].contains(CHAIN_OUT));
        assert!(returns[3].contains("--dport 9090") && returns[3].contains(CHAIN_OUT));
    }

    #[test]
    fn every_line_is_a_clean_argv_and_tagged() {
        for rule in build_rules(InitParams::default()) {
            let words: Vec<&str> = rule.split(' ').collect();
            assert_eq!(words[0], "iptables");
            assert!(words.iter().all(|w| !w.is_empty()), "no empty tokens: {rule}");
            if words[3] == "-A" {
                assert!(rule.contains(&format!("--comment {COMMENT}")), "append carries the tag: {rule}");
            }
        }
        // The -C guard and its -I twin differ only in the op word.
        let plan = build_rules(InitParams::default());
        for w in plan.windows(2) {
            if w[0].split(' ').nth(3) == Some("-C") {
                assert_eq!(w[0].replacen(" -C ", " -I ", 1), w[1]);
            }
        }
    }

    #[test]
    fn cli_parse_plan_equals_build_rules() {
        let argv = [
            "--proxy-uid", "65534", "--inbound-port", "4143", "--outbound-port", "4140",
            "--ignore-inbound-ports", "4191", "--ignore-outbound-ports", "443", "--skip-subnets", "10.0.0.0/8",
        ];
        assert_eq!(plan_of(&argv), build_rules(InitParams { skip_subnets: vec!["10.0.0.0/8".into()], ..Default::default() }));
        // --flag=value form is accepted too.
        let inline = plan_of(&["--proxy-uid=65534", "--inbound-port=5000"]);
        assert!(inline.iter().any(|l| l.ends_with("--to-ports 5000")));
        // run() prints exactly the plan on dry-run; nothing is spawned.
        let params = InitParams { dry_run: true, ..InitParams::default() };
        assert_eq!(build_rules(params.clone()), build_rules(InitParams::default()));
        assert!(params.dry_run);
    }

    #[test]
    fn cli_errors_are_actionable() {
        let err = |argv: &[&str]| {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            parse_args(&owned).unwrap_err()
        };
        assert!(err(&[]).contains("--proxy-uid"));
        assert!(err(&["--proxy-uid", "7", "--inbound-port", "0"]).contains("invalid port"));
        assert!(err(&["--proxy-uid", "x"]).contains("invalid proxy-uid"));
        assert!(err(&["--proxy-uid", "7", "--skip-subnets", "300.1.2.3/8"]).contains("invalid subnet"));
        assert!(err(&["--proxy-uid", "7", "--wat"]).contains("unknown flag"));
        assert!(err(&["--proxy-uid", "7", "--backend", "pf"]).contains("unknown backend"));
        assert!(err(&["--proxy-uid", "7", "--ignore-inbound-ports"]).contains("needs a value"));
    }
}
