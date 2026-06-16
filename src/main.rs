//! `sshelf` — a TUI SSH host manager.
//!
//! The binary runs in one of three modes:
//!  - askpass: when invoked by `ssh` via `SSH_ASKPASS` (detected by the `SSHELF_ASKPASS` env var);
//!  - completion: when invoked by a shell's dynamic-completion hook (the `COMPLETE` env var);
//!  - normal: the interactive TUI (default) or a subcommand (`list`, `add`, `import`, …).

mod app;
mod askpass;
mod config;
mod import;
mod model;
mod paths;
mod search;
mod secrets;
mod ssh;
mod state;
mod store;
mod transfer;
mod ui;
mod vault;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::CompleteEnv;
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use serde::Serialize;

use crate::config::Config;
use crate::model::{AuthMethod, Host};
use crate::paths::{CONFIG_ENV, Paths};
use crate::state::FrecencyState;

#[derive(Parser)]
#[command(
    name = "sshelf",
    version,
    about = "A TUI SSH host manager",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Use a specific config file (default: ~/.config/sshelf/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Append transfer-screen diagnostics (the ssh/sftp commands + their errors; no secrets)
    /// to FILE. Also settable via $SSHELF_TRANSFER_LOG.
    #[arg(long, global = true, value_name = "FILE")]
    transfer_log: Option<PathBuf>,
    /// Connect directly to a saved host by name (skips the TUI), or `-` to reconnect to the
    /// most recently used host. With no host and no subcommand, sshelf launches the TUI.
    #[arg(value_name = "HOST", add = ArgValueCandidates::new(host_name_candidates))]
    host: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List saved hosts (sorted by frecency). Optional QUERY filters by fuzzy text and/or
    /// `tag:NAME` tokens — the same syntax as the TUI search box.
    List {
        #[arg(value_name = "QUERY", num_args = 0..)]
        query: Vec<String>,
        /// Emit machine-readable JSON (host fields + the generated ssh command) instead of the
        /// human table. Always valid JSON, even when the result is empty.
        #[arg(long)]
        json: bool,
    },
    /// Add a host. With no arguments, opens the TUI add form; with arguments, adds it
    /// non-interactively (NAME and --hostname are required).
    Add(AddArgs),
    /// Import hosts from ~/.ssh/config (read-only).
    Import {
        /// Show what would be imported without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Store a password (read from stdin) for a host, by name or id.
    SetPassword {
        /// Host name or id.
        #[arg(add = ArgValueCandidates::new(host_name_candidates))]
        host: String,
    },
    /// Print the generated ssh command for a host without connecting.
    #[command(name = "print-command")]
    Print {
        /// Host name or id.
        #[arg(add = ArgValueCandidates::new(host_name_candidates))]
        host: String,
    },
    /// Print static shell completions to stdout (for packaging). For host-name completion,
    /// set up dynamic completions instead — see the README.
    Completions {
        /// Shell: bash, zsh, fish, elvish, or powershell.
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout.
    Man,
}

/// Flags for non-interactive `sshelf add`. All optional, so bare `sshelf add` opens the TUI.
#[derive(Args, Debug, Default)]
struct AddArgs {
    /// Host alias to search and connect by. Required for non-interactive add.
    name: Option<String>,
    /// IP address or DNS name. Required for non-interactive add.
    #[arg(short = 'H', long)]
    hostname: Option<String>,
    /// Login user (defaults to your $USER at connect time).
    #[arg(short = 'u', long)]
    user: Option<String>,
    /// SSH port (default 22).
    #[arg(short = 'p', long)]
    port: Option<u16>,
    /// Auth method: agent (default), key, or password. Inferred as `key` when --identity is
    /// given, or `password` when --password-stdin is given.
    #[arg(short = 'a', long, value_enum)]
    auth: Option<AuthArg>,
    /// Identity file for key auth (repeatable). Implies `--auth key`.
    #[arg(short = 'i', long = "identity", value_name = "PATH")]
    identity_files: Vec<String>,
    /// ProxyJump host (repeatable or comma-separated). Key/agent auth only in v1.
    #[arg(short = 'J', long = "jump", value_name = "HOST", value_delimiter = ',')]
    jump_hosts: Vec<String>,
    /// Tag for grouping/filtering (repeatable or comma-separated).
    #[arg(short = 't', long = "tag", value_name = "TAG", value_delimiter = ',')]
    tags: Vec<String>,
    /// Extra raw ssh flags, appended verbatim (e.g. "-o BatchMode=yes"). Quote the whole value;
    /// hyphen-leading values are allowed here so the flags pass through.
    #[arg(long = "extra", value_name = "ARGS", allow_hyphen_values = true)]
    extra_args: Option<String>,
    /// Read a password / key passphrase from stdin and store it (OS keyring or age vault),
    /// keeping the secret out of argv and shell history. Implies `--auth password`.
    #[arg(long)]
    password_stdin: bool,
}

/// CLI spelling of the auth method, kept separate from `model::AuthMethod` so the model stays
/// free of a clap dependency.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum AuthArg {
    Agent,
    Key,
    Password,
}

impl From<AuthArg> for AuthMethod {
    fn from(a: AuthArg) -> Self {
        match a {
            AuthArg::Agent => AuthMethod::Agent,
            AuthArg::Key => AuthMethod::Key,
            AuthArg::Password => AuthMethod::Password,
        }
    }
}

impl AddArgs {
    /// True when at least one field was supplied (→ non-interactive add). Bare `sshelf add`
    /// supplies nothing and opens the TUI form instead.
    fn has_args(&self) -> bool {
        self.name.is_some()
            || self.hostname.is_some()
            || self.user.is_some()
            || self.port.is_some()
            || self.auth.is_some()
            || !self.identity_files.is_empty()
            || !self.jump_hosts.is_empty()
            || !self.tags.is_empty()
            || self.extra_args.is_some()
            || self.password_stdin
    }

    /// Effective auth method, with inference from --identity / --password-stdin.
    fn resolved_auth(&self) -> AuthMethod {
        match self.auth {
            Some(a) => a.into(),
            None if !self.identity_files.is_empty() => AuthMethod::Key,
            None if self.password_stdin => AuthMethod::Password,
            None => AuthMethod::Agent,
        }
    }

    /// Build a `Host` from the args (no IO). Errors if NAME or --hostname is missing.
    fn into_host(self) -> Result<Host> {
        let auth = self.resolved_auth();
        let name = self.name.context(
            "non-interactive `add` needs a NAME (run `sshelf add` with no arguments for the form)",
        )?;
        let hostname = self.hostname.context(
            "non-interactive `add` needs --hostname (run `sshelf add` with no arguments for the form)",
        )?;
        let mut host = Host::new(name, hostname);
        host.user = self.user;
        host.port = self.port;
        host.auth = auth;
        host.identity_files = self.identity_files;
        host.jump_hosts = self.jump_hosts;
        host.tags = self.tags;
        host.extra_args = self.extra_args;
        Ok(host)
    }
}

fn main() -> Result<()> {
    // ssh invokes us as `sshelf "<prompt>"` with SSHELF_ASKPASS=1 in the environment.
    // Checked before clap, since the prompt is a positional arg, not a flag.
    if std::env::var_os("SSHELF_ASKPASS").is_some() {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        std::process::exit(askpass::run(&prompt));
    }

    // Dynamic shell completion: if the shell's completion hook invoked us (COMPLETE env var),
    // emit candidates and exit. A no-op on a normal run.
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    // `--config` is plumbed to all paths via the env var so subcommands + Paths resolution see
    // it uniformly. Set before any Paths::resolve().
    if let Some(path) = &cli.config {
        // SAFETY: set once at startup, before any threads are spawned.
        unsafe {
            std::env::set_var(CONFIG_ENV, path);
        }
    }
    if let Some(path) = &cli.transfer_log {
        // SAFETY: set once at startup, before any threads are spawned.
        unsafe {
            std::env::set_var(transfer::LOG_ENV, path);
        }
    }
    match cli.command {
        Some(Command::List { query, json }) => cmd_list(&query.join(" "), json),
        Some(Command::Add(args)) => {
            if args.has_args() {
                cmd_add(args)
            } else {
                app::run_add()
            }
        }
        Some(Command::Import { dry_run }) => cmd_import(dry_run),
        Some(Command::SetPassword { host }) => cmd_set_password(&host),
        Some(Command::Print { host }) => cmd_print_command(&host),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "sshelf", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Man) => clap_mangen::Man::new(Cli::command())
            .render(&mut std::io::stdout())
            .context("rendering man page"),
        // No subcommand: `-` reconnects to the last host, a bare name connects directly,
        // nothing launches the TUI.
        None => match cli.host.as_deref() {
            Some("-") => cmd_connect_last(),
            Some(name) => cmd_connect(name),
            None => app::run(),
        },
    }
}

fn cmd_import(dry_run: bool) -> Result<()> {
    let path = import::default_config_path().context("HOME is not set")?;
    if !path.exists() {
        anyhow::bail!("no ssh config at {}", path.display());
    }
    let result = import::parse_file(&path)?;
    println!(
        "Parsed {} host(s) from {}",
        result.hosts.len(),
        path.display()
    );
    for w in &result.warnings {
        println!("  warning: {w}");
    }

    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let cfg = Config::load(&paths.config_file())?;
    let hosts_path = cfg.hosts_path(&paths);
    let mut file = store::load_hosts(&hosts_path)?;
    let to_add = import::new_hosts(&result.hosts, &file.hosts)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    if to_add.is_empty() {
        println!("Nothing new to import (all names already exist).");
        return Ok(());
    }
    println!("{} new host(s):", to_add.len());
    for h in &to_add {
        println!("  {:<20} {}", h.name, h.endpoint());
    }
    if dry_run {
        println!("(dry run — nothing written)");
        return Ok(());
    }
    file.hosts.extend(to_add);
    store::save_hosts(&hosts_path, &file)?;
    println!("Imported into {}", hosts_path.display());
    Ok(())
}

fn cmd_set_password(host_ref: &str) -> Result<()> {
    use std::io::BufRead;
    let paths = Paths::resolve()?;
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;
    let host = hosts
        .iter()
        .find(|h| h.id == host_ref || h.name == host_ref)
        .with_context(|| format!("no host with name or id '{host_ref}'"))?;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    let password = line.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        anyhow::bail!("empty password; nothing stored");
    }
    secrets::store_password(&paths.vault_file(), &host.id, password)?;
    println!("stored password for \"{}\" ({})", host.name, host.id);
    Ok(())
}

fn cmd_print_command(host_ref: &str) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;
    let host = resolve_host(&hosts, host_ref)
        .with_context(|| format!("no host with name or id '{host_ref}'"))?;
    println!("{}", ssh::command_string(host));
    Ok(())
}

fn cmd_list(query: &str, json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts_path = cfg.hosts_path(&paths);
    let hosts = store::load_hosts(&hosts_path)?.hosts;
    let st = FrecencyState::load(&paths.state_file())?;
    let order = search::rank(&hosts, query, &st, cfg.decay_rate, cfg.default_sort);

    // JSON must always be valid (even empty) for scripts — emit before the human messages.
    if json {
        let selected: Vec<&Host> = order.iter().map(|&i| &hosts[i]).collect();
        println!("{}", hosts_to_json(&selected)?);
        return Ok(());
    }

    if hosts.is_empty() {
        println!("No hosts yet. Add one with `sshelf add`, or create:");
        println!("  {}", hosts_path.display());
        return Ok(());
    }
    if order.is_empty() {
        println!("No hosts match '{}'.", query.trim());
        return Ok(());
    }
    for &i in &order {
        let h = &hosts[i];
        let tags = if h.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", h.tags.join(", "))
        };
        println!(
            "{:<20}  {:<28}  {}{}",
            h.name,
            h.endpoint(),
            h.auth.as_str(),
            tags
        );
    }
    Ok(())
}

/// A host record plus its generated ssh command, for `sshelf list --json`.
#[derive(Serialize)]
struct HostJson<'a> {
    #[serde(flatten)]
    host: &'a Host,
    /// The ready-to-run ssh command (sshelf's value-add for scripts and integrations).
    command: String,
}

fn hosts_to_json(hosts: &[&Host]) -> Result<String> {
    let items: Vec<HostJson> = hosts
        .iter()
        .map(|&h| HostJson {
            host: h,
            command: ssh::command_string(h),
        })
        .collect();
    serde_json::to_string_pretty(&items).context("serializing hosts to JSON")
}

/// Add a host non-interactively from CLI flags.
fn cmd_add(args: AddArgs) -> Result<()> {
    let auth = args.resolved_auth();
    let read_secret = args.password_stdin;
    let host = args.into_host()?;

    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts_path = cfg.hosts_path(&paths);
    let mut file = store::load_hosts(&hosts_path)?;
    if file.hosts.iter().any(|h| h.name == host.name) {
        anyhow::bail!(
            "a host named '{}' already exists — pick another name (or edit it in the TUI)",
            host.name
        );
    }

    // Read the secret before writing anything, so empty stdin can't leave a half-added host.
    let secret = if read_secret {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("reading secret from stdin")?;
        let s = line.trim_end_matches(['\n', '\r']).to_string();
        if s.is_empty() {
            anyhow::bail!("--password-stdin given but stdin was empty; nothing added");
        }
        Some(s)
    } else {
        None
    };

    let id = host.id.clone();
    let name = host.name.clone();
    file.hosts.push(host);
    store::save_hosts(&hosts_path, &file)?;
    if let Some(s) = &secret {
        secrets::store_password(&paths.vault_file(), &id, s)?;
    }
    println!("added '{name}' ({id})");
    if auth == AuthMethod::Password && secret.is_none() {
        println!(
            "note: password auth set but no password stored — run `sshelf set-password {name}`"
        );
    }
    Ok(())
}

/// Connect directly to a host by name or id, skipping the TUI.
fn cmd_connect(host_ref: &str) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;

    let Some(host) = resolve_host(&hosts, host_ref).cloned() else {
        let st = FrecencyState::load(&paths.state_file()).unwrap_or_default();
        let order = search::rank(&hosts, host_ref, &st, cfg.decay_rate, cfg.default_sort);
        if order.is_empty() {
            anyhow::bail!("no host named '{host_ref}' — run `sshelf list` to see your hosts");
        }
        let names: Vec<&str> = order
            .iter()
            .take(5)
            .map(|&i| hosts[i].name.as_str())
            .collect();
        anyhow::bail!(
            "no host named '{host_ref}' — did you mean: {}",
            names.join(", ")
        );
    };
    connect(&host, &paths)
}

/// Reconnect to the most-recently-used host (`sshelf -`).
fn cmd_connect_last() -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;
    let st = FrecencyState::load(&paths.state_file())?;
    let id = last_used_id(&st)
        .context("no recent host yet — connect to one first, then `sshelf -` reconnects to it")?;
    let host = hosts
        .iter()
        .find(|h| h.id == id)
        .cloned()
        .context("the last-used host is no longer in your list — run `sshelf list`")?;
    connect(&host, &paths)
}

/// Record frecency BEFORE `exec()` (nothing runs after a successful exec), wire `SSH_ASKPASS`
/// only when a secret is stored, then replace this process with `ssh`.
fn connect(host: &Host, paths: &Paths) -> Result<()> {
    let mut st = FrecencyState::load(&paths.state_file())?;
    st.record_use(&host.id);
    if let Err(e) = st.save(&paths.state_file()) {
        eprintln!("sshelf: warning: could not save state: {e:#}");
    }
    let has_secret = secrets::get_password(&paths.vault_file(), &host.id)
        .ok()
        .flatten()
        .is_some();
    // Replaces this process on success; returns only on failure.
    Err(ssh::exec_connect(host, has_secret))
}

/// Find a host by exact name or id.
fn resolve_host<'a>(hosts: &'a [Host], reference: &str) -> Option<&'a Host> {
    hosts
        .iter()
        .find(|h| h.name == reference || h.id == reference)
}

/// The id of the most-recently-used host, if any.
fn last_used_id(state: &FrecencyState) -> Option<String> {
    state
        .stats
        .iter()
        .max_by_key(|(_, s)| s.last_used)
        .map(|(id, _)| id.clone())
}

/// Completion candidates for a host-name argument: each saved host's name, with its endpoint
/// as the description. Best-effort and side-effect-free (any error → no candidates).
fn host_name_candidates() -> Vec<CompletionCandidate> {
    let Ok(paths) = Paths::resolve() else {
        return Vec::new();
    };
    let Ok(cfg) = Config::load(&paths.config_file()) else {
        return Vec::new();
    };
    match store::load_hosts(&cfg.hosts_path(&paths)) {
        Ok(file) => host_candidates_from(&file.hosts),
        Err(_) => Vec::new(),
    }
}

fn host_candidates_from(hosts: &[Host]) -> Vec<CompletionCandidate> {
    hosts
        .iter()
        .map(|h| CompletionCandidate::new(&h.name).help(Some(h.endpoint().into())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_routes_host_vs_subcommand() {
        // A known subcommand name wins.
        let c = Cli::try_parse_from(["sshelf", "list"]).unwrap();
        assert!(matches!(c.command, Some(Command::List { .. })));
        assert!(c.host.is_none());

        // `add` is a subcommand, not a host called "add".
        let c = Cli::try_parse_from(["sshelf", "add"]).unwrap();
        assert!(matches!(c.command, Some(Command::Add(_))));
        assert!(c.host.is_none());

        // A bare, unknown token is the host positional → direct connect.
        let c = Cli::try_parse_from(["sshelf", "prod-web"]).unwrap();
        assert!(c.command.is_none());
        assert_eq!(c.host.as_deref(), Some("prod-web"));

        // Nothing → TUI.
        let c = Cli::try_parse_from(["sshelf"]).unwrap();
        assert!(c.command.is_none() && c.host.is_none());
    }

    #[test]
    fn dash_parses_as_host_for_reconnect() {
        let c = Cli::try_parse_from(["sshelf", "-"]).unwrap();
        assert_eq!(c.host.as_deref(), Some("-"));
        assert!(c.command.is_none());
    }

    #[test]
    fn list_query_and_json_flag() {
        let c = Cli::try_parse_from(["sshelf", "list", "--json", "tag:web", "staging"]).unwrap();
        match c.command {
            Some(Command::List { query, json }) => {
                assert!(json);
                assert_eq!(query, vec!["tag:web", "staging"]);
            }
            _ => panic!("expected the list subcommand"),
        }
    }

    #[test]
    fn print_command_captures_host() {
        let c = Cli::try_parse_from(["sshelf", "print-command", "prod-web"]).unwrap();
        match c.command {
            Some(Command::Print { host }) => assert_eq!(host, "prod-web"),
            _ => panic!("expected the print-command subcommand"),
        }
    }

    #[test]
    fn add_alone_opens_the_tui() {
        let c = Cli::try_parse_from(["sshelf", "add"]).unwrap();
        match c.command {
            Some(Command::Add(a)) => assert!(!a.has_args()),
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn add_with_flags_builds_a_host() {
        let c = Cli::try_parse_from([
            "sshelf",
            "add",
            "prod-db",
            "-H",
            "10.0.0.2",
            "-u",
            "mike",
            "-p",
            "5432",
            "-i",
            "~/.ssh/k",
            "-J",
            "bastion",
            "-t",
            "prod,db",
            "--extra",
            "-o BatchMode=yes",
        ])
        .unwrap();
        let args = match c.command {
            Some(Command::Add(a)) => a,
            _ => panic!("expected add"),
        };
        assert!(args.has_args());
        let h = args.into_host().unwrap();
        assert_eq!(h.name, "prod-db");
        assert_eq!(h.hostname, "10.0.0.2");
        assert_eq!(h.user.as_deref(), Some("mike"));
        assert_eq!(h.port, Some(5432));
        assert_eq!(h.auth, AuthMethod::Key); // inferred from --identity
        assert_eq!(h.identity_files, vec!["~/.ssh/k".to_string()]);
        assert_eq!(h.jump_hosts, vec!["bastion".to_string()]);
        assert_eq!(h.tags, vec!["prod".to_string(), "db".to_string()]); // comma split
        assert_eq!(h.extra_args.as_deref(), Some("-o BatchMode=yes"));
    }

    #[test]
    fn add_requires_name_and_hostname() {
        let c = Cli::try_parse_from(["sshelf", "add", "--hostname", "h"]).unwrap();
        let args = match c.command {
            Some(Command::Add(a)) => a,
            _ => panic!("expected add"),
        };
        assert!(args.has_args());
        assert!(args.into_host().is_err()); // missing NAME
    }

    #[test]
    fn add_password_stdin_infers_password_auth() {
        let c =
            Cli::try_parse_from(["sshelf", "add", "box", "-H", "h", "--password-stdin"]).unwrap();
        match c.command {
            Some(Command::Add(a)) => assert_eq!(a.resolved_auth(), AuthMethod::Password),
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn global_config_works_with_a_host() {
        let c = Cli::try_parse_from(["sshelf", "--config", "/tmp/x.toml", "prod-web"]).unwrap();
        assert_eq!(c.host.as_deref(), Some("prod-web"));
        assert_eq!(
            c.config.as_deref(),
            Some(std::path::Path::new("/tmp/x.toml"))
        );
    }

    #[test]
    fn resolve_host_by_name_and_id() {
        let mut h = Host::new("prod-web", "10.0.0.1");
        h.id = "abc123".into();
        let hosts = vec![h];
        assert!(resolve_host(&hosts, "prod-web").is_some());
        assert!(resolve_host(&hosts, "abc123").is_some());
        assert!(resolve_host(&hosts, "missing").is_none());
    }

    #[test]
    fn last_used_id_picks_most_recent() {
        let mut st = FrecencyState::default();
        st.stats.insert(
            "old".into(),
            crate::state::HostStat {
                use_count: 9,
                last_used: 100,
            },
        );
        st.stats.insert(
            "new".into(),
            crate::state::HostStat {
                use_count: 1,
                last_used: 200,
            },
        );
        assert_eq!(last_used_id(&st).as_deref(), Some("new"));
        assert!(last_used_id(&FrecencyState::default()).is_none());
    }

    #[test]
    fn json_output_has_fields_and_command() {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("root".into());
        let refs = vec![&h];
        let j = hosts_to_json(&refs).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed[0]["name"], "web");
        assert_eq!(parsed[0]["hostname"], "10.0.0.1");
        assert!(
            parsed[0]["command"]
                .as_str()
                .unwrap()
                .contains("root@10.0.0.1")
        );
    }

    #[test]
    fn host_candidates_from_lists_each_name() {
        let hosts = vec![Host::new("a", "h1"), Host::new("b", "h2")];
        assert_eq!(host_candidates_from(&hosts).len(), 2);
    }
}
