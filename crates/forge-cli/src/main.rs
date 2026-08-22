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
        dir: Option<PathBuf>,
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
        dir: PathBuf,
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

fn main() -> ExitCode {
    match run() {
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
        Error::Denied(_) | Error::Cap(_) | Error::Invalid(_) | Error::InvalidBase | Error::NotFound(_) => 1,
        Error::Corrupt(_) | Error::Sealed(_) => 2,
        Error::Busy(_) => 3,
        Error::StaleObservation { .. } | Error::MergeConflict(_) => 4,
        Error::Io(_) | Error::Sqlite(_) | Error::Internal(_) => 5,
    }
}

fn run() -> forge_types::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Bench {
            agents,
            shared,
            workers,
        } => {
            let dir = cli.dir.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("forge-bench-{}", std::process::id()))
            });
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            eprintln!(
                "forge bench dir={} agents={agents} shared={shared} workers={workers}",
                dir.display()
            );
            let report = forge_api::run_bench_with_workers(&dir, agents, shared, workers)?;
            print!("{}", report.render());
            Ok(())
        }
        Cmd::Init { dir } => {
            let dir = dir
                .or(cli.dir.clone())
                .unwrap_or_else(|| PathBuf::from("."));
            let f = Forge::init(&dir)?;
            println!("initialized {}", f.root().display());
            println!("root cap: {}", f.root().join("keys/root.cap").display());
            Ok(())
        }
        Cmd::Serve { http } => {
            let f = Arc::new(open(&cli)?);
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

fn open(cli: &Cli) -> forge_types::Result<Forge> {
    let dir = cli.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    Forge::open(&dir)
}

fn load_cap(f: &Forge, cli: &Cli) -> forge_types::Result<Cap> {
    if let Some(c) = &cli.cap {
        let tok = if Path::new(c).is_file() {
            std::fs::read_to_string(c)?
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
        Cmd::Import { dir, r#ref } => {
            let id = f.import_dir(cap, &dir, &r#ref)?;
            println!("imported {id} -> {ref_name}", ref_name = r#ref);
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
            for r in f.refs(cap)? {
                let flags = format!(
                    "{}{}",
                    if r.protected { "P" } else { "-" },
                    if r.sealed { "S" } else { "-" }
                );
                println!("{flags} {:<32} {} {}", r.kind, r.name, r.oid);
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
