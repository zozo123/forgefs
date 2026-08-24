use clap::{Parser, Subcommand};
use forge_api::Forge;
use forge_cap::Cap;
use forge_types::{CasResult, Error, ObjectId};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "forge",
    version,
    about = "Content-addressable filesystem for agents"
)]
struct Cli {
    /// Forge directory (or a parent of .forge). Env FORGE_DIR.
    #[arg(long, global = true, env = "FORGE_DIR")]
    dir: Option<PathBuf>,
    /// Capability token or path. Env FORGE_CAP.
    #[arg(long, global = true, env = "FORGE_CAP")]
    cap: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Init {
        /// Where to create the forge. Not named `dir` for the same reason as
        /// `import`: it would collide with the global `--dir` arg id.
        path: Option<PathBuf>,
    },
    Serve {
        #[arg(long)]
        http: bool,
    },
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    Mount {
        #[arg(long)]
        ns: String,
        path: String,
        spec: String,
        #[arg(long)]
        rw: bool,
    },
    Ls {
        #[arg(long)]
        ns: String,
        path: Option<String>,
    },
    Read {
        #[arg(long)]
        ns: String,
        path: String,
    },
    Write {
        #[arg(long)]
        ns: String,
        path: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        text: Option<String>,
    },
    Checkin {
        #[arg(long)]
        ns: String,
        #[arg(short, long, default_value = "")]
        message: String,
    },
    Import {
        /// Directory to import. Deliberately NOT named `dir`: the global
        /// `--dir` (env FORGE_DIR) owns that clap arg id, and a subcommand
        /// field of the same name silently overwrites it, so the repository
        /// would be chosen by this path instead of by --dir.
        source: PathBuf,
        #[arg(long)]
        r#ref: String,
    },
    Branch {
        from: String,
        name: String,
    },
    Merge {
        #[arg(long)]
        into: String,
        #[arg(long)]
        from: String,
        /// Reserved for conflict-bound resolution; raw tree OIDs are rejected.
        #[arg(long)]
        resolved: Option<String>,
    },
    Grant {
        #[arg(long)]
        ops: String,
        #[arg(long)]
        r#ref: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    Inbox {
        #[command(subcommand)]
        cmd: InboxCmd,
    },
    Landmark {
        oid: String,
    },
    Seal {
        r#ref: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        attest: bool,
    },
    Export {
        spec: String,
        #[arg(short, long)]
        output: PathBuf,
    },
    Verify {
        tag: String,
    },
    Refs,
    Log {
        r#ref: String,
    },
    Show {
        spec: String,
    },
    /// Verify repository metadata and durable object integrity without repair.
    Fsck {
        /// Scan every object file, including unreachable/orphan objects.
        #[arg(long)]
        full: bool,
        /// Emit the structured report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Timed concurrent checkin / shared-ref stampede / merge+seal+verify.
    Bench {
        /// New, dedicated benchmark workspace to preserve after the run.
        /// Must not already exist. Omit to use an owned temporary directory.
        #[arg(long)]
        scratch: Option<PathBuf>,
        #[arg(long, default_value_t = 32)]
        agents: usize,
        #[arg(long, default_value_t = 16)]
        shared: usize,
        /// Maximum OS workers driving logical agents.
        #[arg(long, default_value_t = 64)]
        workers: usize,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    Open {
        #[arg(long, default_value = "main")]
        from: String,
    },
}

#[derive(Subcommand)]
enum InboxCmd {
    /// Publish a sealed snapshot to a recipient-owned inbox ref.
    Push {
        #[arg(long)]
        to: String,
        #[arg(long)]
        snapshot: String,
    },
    /// List inbox refs owned by the calling capability's agent.
    List,
}

fn main() -> ExitCode {
    restore_default_sigpipe();

    // clap exits the process itself on a usage error, with its own default code
    // 2 -- which CLI_ABI.md reserves for "corruption or sealed-state violation".
    // Parse explicitly so a typo is reported as the input error it is, and only
    // an explicit --help/--version succeeds.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("forge: {e}");
            ExitCode::from(error_exit_code(&e))
        }
    }
}

/// Stable CLI error ABI for agents and shell callers. Never key automation on prose.
fn error_exit_code(error: &Error) -> u8 {
    match error {
        Error::Denied(_)
        | Error::Cap(_)
        | Error::Invalid(_)
        | Error::InvalidBase
        | Error::NotFound(_) => 1,
        Error::Corrupt(_) | Error::Sealed(_) => 2,
        Error::Busy(_) => 3,
        Error::StaleObservation { .. } | Error::MergeConflict(_) => 4,
        Error::Io(_) | Error::Sqlite(_) | Error::Internal(_) => 5,
    }
}

/// Rust sets SIGPIPE to SIG_IGN, so a closed reader (`forge ... | head`) makes
/// the next `eprintln!` fail and panic, exiting 101 -- a code that appears
/// nowhere in CLI_ABI.md. Restore the default so the shell contract holds.
fn restore_default_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn run(cli: Cli) -> forge_types::Result<()> {
    match cli.cmd {
        Cmd::Bench {
            scratch,
            agents,
            shared,
            workers,
        } => {
            if cli.dir.is_some() {
                return Err(Error::Invalid(
                    "bench does not accept --dir/FORGE_DIR; use --scratch <new-path> or omit it"
                        .into(),
                ));
            }
            let (_scratch_guard, dir) = match scratch {
                Some(dir) => {
                    match std::fs::symlink_metadata(&dir) {
                        Ok(_) => {
                            return Err(Error::Invalid(format!(
                                "benchmark scratch path already exists: {}",
                                dir.display()
                            )))
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    (None, dir)
                }
                None => {
                    let guard = tempfile::Builder::new().prefix("forge-bench-").tempdir()?;
                    let dir = guard.path().to_path_buf();
                    (Some(guard), dir)
                }
            };
            eprintln!(
                "forge bench dir={} agents={agents} shared={shared} workers={workers}",
                dir.display()
            );
            let report = forge_api::run_bench_with_workers(&dir, agents, shared, workers)?;
            print!("{}", report.render());
            Ok(())
        }
        Cmd::Init { path } => {
            let dir = path
                .or(cli.dir.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let f = Forge::init(&dir)?;
            println!("initialized {}", f.root().display());
            println!("root cap: {}", f.root().join("keys/root.cap").display());
            Ok(())
        }
        Cmd::Serve { http } => {
            let dir = cli.dir.clone().unwrap_or_else(|| PathBuf::from("."));
            let f = Arc::new(Forge::open_for_serve(&dir)?);
            eprintln!("forge serve {}", f.root().display());
            forge_api::serve(f, http)
        }
        _ => {
            let f = open(&cli)?;
            let cap = load_cap(&f, &cli)?;
            dispatch(&f, &cap, cli.cmd)
        }
    }
}

/// `fsck` and `verify` are documented as read-only, so they take structurally
/// read-only opens unconditionally -- not only when the media happens to be
/// read-only. Full fsck alone defers migration-ledger compatibility to its
/// catalog audit so it can report the defect; reachable fsck and verify retain
/// the strict compatible-schema open. SQLite refuses every write in all three
/// modes. Every other command opens for writing exactly as before.
fn open(cli: &Cli) -> forge_types::Result<Forge> {
    let dir = cli.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    match &cli.cmd {
        Cmd::Fsck { full, .. } => Forge::open_for_fsck(&dir, *full),
        Cmd::Verify { .. } => Forge::open_read_only(&dir),
        _ => Forge::open(&dir),
    }
}

fn load_cap(f: &Forge, cli: &Cli) -> forge_types::Result<Cap> {
    if let Some(c) = &cli.cap {
        let tok = if Path::new(c).is_file() {
            // Not read_to_string: its InvalidData becomes Error::Io -> exit 5,
            // while forge-cap maps the same failure to Error::Cap -> exit 1.
            let bytes = std::fs::read(c)?;
            String::from_utf8(bytes)
                .map_err(|_| Error::Cap(format!("capability file {c} is not valid UTF-8")))?
        } else {
            c.clone()
        };
        return f.load_cap(tok.trim());
    }
    Err(Error::Denied(
        "pass --cap PATH|TOKEN or FORGE_CAP; no ambient root capability".into(),
    ))
}

fn dispatch(f: &Forge, cap: &Cap, cmd: Cmd) -> forge_types::Result<()> {
    match cmd {
        Cmd::Session {
            cmd: SessionCmd::Open { from },
        } => {
            let ns = f.session_open(cap, &from)?;
            println!("{ns}");
        }
        Cmd::Mount { ns, path, spec, rw } => {
            f.mount(cap, &ns, &path, &spec, rw)?;
            println!("mounted {path} -> {spec}");
        }
        Cmd::Ls { ns, path } => {
            for (n, k, id, x) in f.ls(cap, &ns, path.as_deref().unwrap_or("/"))? {
                let x = if x { "x" } else { "-" };
                println!("{k:5} {x} {id} {n}");
            }
        }
        Cmd::Read { ns, path } => {
            let b = f.read(cap, &ns, &path)?;
            std::io::Write::write_all(&mut std::io::stdout(), &b).ok();
        }
        Cmd::Write {
            ns,
            path,
            file,
            text,
        } => {
            let data = if let Some(p) = file {
                if !p.is_file() {
                    return Err(Error::Invalid(format!(
                        "--file {} is not a readable file",
                        p.display()
                    )));
                }
                std::fs::read(p)?
            } else if let Some(t) = text {
                t.into_bytes()
            } else {
                return Err(Error::Invalid("write needs --file or --text".into()));
            };
            let id = f.write(cap, &ns, &path, &data, false)?;
            println!("{id}");
        }
        Cmd::Checkin { ns, message } => match f.checkin(cap, &ns, "/", &message)? {
            CasResult::Updated { name, oid } => println!("updated {name} {oid}"),
            CasResult::Forked {
                requested,
                fork,
                ours,
                theirs,
            } => println!("forked {requested} -> {fork} ours={ours} theirs={theirs}"),
            CasResult::Noop { name, oid } => println!("noop {name} {oid}"),
        },
        Cmd::Import { source, r#ref } => {
            if !source.is_dir() {
                return Err(Error::Invalid(format!(
                    "import source {} is not a directory",
                    source.display()
                )));
            }
            match f.import_dir(cap, &source, &r#ref)? {
                CasResult::Updated { name, oid } => println!("imported {oid} -> {name}"),
                CasResult::Forked {
                    requested,
                    fork,
                    ours,
                    theirs,
                } => println!("forked {requested} -> {fork} ours={ours} theirs={theirs}"),
                CasResult::Noop { name, oid } => println!("noop {name} {oid}"),
            }
        }
        Cmd::Branch { from, name } => {
            let id = f.branch(cap, &from, &name)?;
            println!("{name} {id}");
        }
        Cmd::Merge {
            into,
            from,
            resolved,
        } => {
            let res = resolved.as_deref().map(ObjectId::from_hex).transpose()?;
            match f.merge(cap, &into, &from, res) {
                Ok(CasResult::Updated { name, oid }) => println!("merged {name} {oid}"),
                Ok(CasResult::Forked { fork, .. }) => println!("merge forked {fork}"),
                Ok(other) => println!("{other:?}"),
                Err(Error::MergeConflict(oid)) => {
                    eprintln!("conflict {oid}");
                    return Err(Error::MergeConflict(oid));
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Grant { ops, r#ref, agent } => {
            let mut extra = vec![format!("ops={ops}")];
            if let Some(r) = r#ref {
                extra.push(format!("ref={r}"));
            }
            if let Some(a) = agent {
                extra.push(format!("agent={a}"));
            }
            let c = f.grant(cap, extra)?;
            println!("{}", c.to_token());
        }
        Cmd::Inbox {
            cmd: InboxCmd::Push { to, snapshot },
        } => match f.inbox_push(cap, &to, &snapshot)? {
            CasResult::Updated { name, oid } => println!("pushed {name} {oid}"),
            CasResult::Forked {
                requested,
                fork,
                ours,
                theirs,
            } => println!("forked {requested} -> {fork} ours={ours} theirs={theirs}"),
            CasResult::Noop { name, oid } => println!("noop {name} {oid}"),
        },
        Cmd::Inbox {
            cmd: InboxCmd::List,
        } => {
            for row in f.inbox_list(cap)? {
                println!("{} {}", row.name, row.oid);
            }
        }
        Cmd::Landmark { oid } => {
            f.landmark(cap, ObjectId::from_hex(&oid)?)?;
            println!("landmark {oid}");
        }
        Cmd::Seal { r#ref, tag, attest } => {
            let oid = f.seal(cap, &r#ref, &tag)?;
            println!("sealed tags/{tag} {oid}");
            if attest {
                f.verify_tag(cap, &tag)?;
                println!("attested ok");
            }
        }
        Cmd::Export { spec, output } => {
            f.export_tar(cap, &spec, &output)?;
            println!("wrote {}", output.display());
        }
        Cmd::Verify { tag } => {
            let oid = f.verify_tag(cap, &tag)?;
            println!("ok {oid}");
        }
        Cmd::Refs => {
            let (refs, suppressed) = f.refs_with_suppressed(cap)?;
            for r in refs {
                let flags = format!(
                    "{}{}",
                    if r.protected { "P" } else { "-" },
                    if r.sealed { "S" } else { "-" }
                );
                println!("{flags} {:<32} {} {}", r.kind, r.name, r.oid);
            }
            if suppressed > 0 {
                eprintln!("{suppressed} ref(s) suppressed by authority");
            }
        }
        Cmd::Log { r#ref } => {
            for (oid, agent, reason) in f.log(cap, &r#ref, 32)? {
                println!("{reason:8} {agent:12} {oid}");
            }
        }
        Cmd::Show { spec } => {
            println!("{}", f.show(cap, &spec)?);
        }
        Cmd::Fsck { full, json } => {
            let report = f.fsck(cap, full)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|e| Error::Internal(e.to_string()))?
                );
            } else {
                let mode = if full { "full" } else { "reachable" };
                println!(
                    "{} ({mode}): {} refs, {} objects, {} namespaces",
                    if report.ok { "ok" } else { "FAILED" },
                    report.checked_refs,
                    report.checked_objects,
                    report.checked_namespaces
                );
                for finding in &report.findings {
                    println!(
                        "[{}] {}: {}",
                        finding.code, finding.resource, finding.detail
                    );
                }
            }
            if !report.ok {
                return Err(Error::Corrupt(format!(
                    "fsck found {} problem(s)",
                    report.findings.len()
                )));
            }
        }
        Cmd::Init { .. } | Cmd::Serve { .. } | Cmd::Bench { .. } => unreachable!(),
    }
    Ok(())
}
