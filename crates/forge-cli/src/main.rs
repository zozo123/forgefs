use clap::{Parser, Subcommand};
use forge_api::{ExportOptions, Forge, ImportOptions};
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
    /// Move a file or directory inside one mount, staged atomically (I24).
    ///
    /// Not a copy and not two commands: the destination and the source
    /// tombstone are staged in a single catalog transaction, so no reader and
    /// no crash sees the content at both paths or at neither. Publication is
    /// still `checkin`, which folds this mount into one Contribution and one
    /// CAS exactly as before.
    Mv {
        #[arg(long)]
        ns: String,
        from: String,
        to: String,
        /// Refuse -- exit 4 -- unless the source resolves to this object id.
        /// The move's assumption about what it is moving, stated so it can be
        /// checked rather than assumed.
        #[arg(long)]
        expect_oid: Option<String>,
    },
    Checkin {
        #[arg(long)]
        ns: String,
        /// Mount to publish. Checkin folds exactly this one mount and CASes the
        /// ref that mount names, from that mount's own pinned base (I19), so a
        /// session that wrote through a `--rw` mount elsewhere names it here.
        #[arg(long, default_value = "/")]
        mount: String,
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
        /// Materialise each symlink as the CONTENT of its target: a file link
        /// becomes a regular file, a directory link becomes a directory. Off by
        /// default because a VERSION 1 tree cannot represent a symlink, so this
        /// is a lossy conversion the operator has to ask for. Targets that
        /// resolve outside the import root are still refused.
        #[arg(long)]
        follow_symlinks: bool,
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
    /// Explicitly retire a fork ref or a session so it stops being a GC root.
    ///
    /// This is the resolution half of I18: a refused checkin forks and keeps
    /// the work, and nothing could ever retire the fork afterwards. It is also
    /// the escape hatch for a session that can no longer make progress.
    Abandon {
        #[command(subcommand)]
        cmd: AbandonCmd,
    },
    /// Report what garbage collection would reclaim, or reclaim it.
    Gc {
        /// Report only. Exactly one of --dry-run and --collect is required.
        #[arg(long)]
        dry_run: bool,
        /// Unlink unreachable objects. See docs/GC.md for the invariant this
        /// preserves and the one precondition it cannot prove.
        #[arg(long)]
        collect: bool,
        /// Unreachable objects younger than this are withheld, because an
        /// object is durable before the catalog row that roots it (I4).
        #[arg(long, default_value_t = forge_api::DEFAULT_MIN_AGE_SECS)]
        min_age_secs: u64,
        /// Emit the structured report as JSON.
        #[arg(long)]
        json: bool,
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
        /// Write the archive even when sibling names collide under case
        /// folding or Unicode NFC/NFD. Off by default: such an archive loses
        /// an entry silently when extracted on macOS or Windows (I16).
        #[arg(long)]
        allow_name_collisions: bool,
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
    /// Report this process lifetime counters for objects, SQLite, ref CAS,
    /// sessions and merges. Never a per-operation measurement.
    Stats {
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
enum AbandonCmd {
    /// Retire a fork ref -- `heads/agents/<agent>/forks/*` for a session
    /// fork, `forks/*` for a merge or import one. The commit stays addressable
    /// by OID and the reflog keeps the record; only the root goes away.
    Fork { r#ref: String },
    /// Retire a session's pin, mounts, overlay and observations.
    Session {
        ns: String,
        /// Required when the session still holds staged overlay entries.
        /// Without it a session with staged work is refused, not emptied.
        #[arg(long)]
        discard_staged: bool,
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
            ExitCode::from(e.exit_code())
        }
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
        Cmd::Mv {
            ns,
            from,
            to,
            expect_oid,
        } => {
            let expect = expect_oid.as_deref().map(ObjectId::from_hex).transpose()?;
            let r = f.rename(cap, &ns, &from, &to, expect)?;
            println!(
                "moved {} {} {} {} entries={}",
                r.from, r.to, r.kind, r.source, r.entries
            );
        }
        Cmd::Checkin { ns, mount, message } => match f.checkin(cap, &ns, &mount, &message)? {
            CasResult::Updated { name, oid } => println!("updated {name} {oid}"),
            CasResult::Forked {
                requested,
                fork,
                ours,
                theirs,
            } => println!("forked {requested} -> {fork} ours={ours} theirs={theirs}"),
            CasResult::Noop { name, oid } => println!("noop {name} {oid}"),
        },
        Cmd::Import {
            source,
            r#ref,
            follow_symlinks,
        } => {
            if !source.is_dir() {
                return Err(Error::Invalid(format!(
                    "import source {} is not a directory",
                    source.display()
                )));
            }
            match f.import_dir_with(cap, &source, &r#ref, ImportOptions { follow_symlinks })? {
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
        Cmd::Abandon {
            cmd: AbandonCmd::Fork { r#ref },
        } => {
            let retired = f.abandon_fork(cap, &r#ref)?;
            println!(
                "abandoned {} {} {}",
                retired.name, retired.kind, retired.oid
            );
        }
        Cmd::Abandon {
            cmd: AbandonCmd::Session { ns, discard_staged },
        } => {
            let retired = f.abandon_session(cap, &ns, discard_staged)?;
            println!(
                "abandoned session {} discarded={} mounts={} observations={}",
                retired.ns_id,
                retired.discarded_overlay,
                retired.removed_mounts,
                retired.removed_observations
            );
        }
        Cmd::Gc {
            dry_run,
            collect,
            min_age_secs,
            json,
        } => {
            let report = match (dry_run, collect) {
                (true, true) => {
                    return Err(Error::Invalid(
                        "gc takes --dry-run or --collect, never both".into(),
                    ))
                }
                (_, true) => f.gc_collect(cap, min_age_secs)?,
                // `gc` with neither flag still refuses -- with a diagnostic that
                // names both modes, rather than the "collection is not
                // implemented" it carried from #12 to #356.
                (dry_run, false) => f.gc(cap, dry_run, min_age_secs)?,
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|e| Error::Internal(e.to_string()))?
                );
            } else {
                print!("{}", report.render());
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
        Cmd::Export {
            spec,
            output,
            allow_name_collisions,
        } => {
            f.export_tar_with(
                cap,
                &spec,
                &output,
                ExportOptions {
                    allow_name_collisions,
                },
            )?;
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
            // `--json` promises a document for every outcome this command
            // has, and #348 turned "this catalog is from an older release"
            // from a rare outcome into a routine one. It used to produce an
            // empty stdout, a paragraph of English on stderr and exit 1, so
            // the one consumer that most needs to tell "not audited" from
            // "audited and clean" -- a script -- was the only one given
            // nothing to read (issue #356). It gets a refusal document
            // instead, distinguishable from a report by its `schema` field
            // and its absence of counters, on stdout where its report would
            // have been. The exit code does not move: this is still exit 1,
            // the same refusal, said in the format that was asked for.
            if json {
                if let Some(refusal) = f.fsck_refusal(cap, full)? {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&refusal)
                            .map_err(|e| Error::Internal(e.to_string()))?
                    );
                    return Err(Error::Invalid(refusal.detail));
                }
            }
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
        // I14: no ambient authority. The counters name no object and no ref,
        // but the command still runs behind a loaded capability so it cannot
        // become an unauthenticated read of a repository durability policy.
        Cmd::Stats { json } => {
            let report = f.stats_report();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .map_err(|e| Error::Internal(e.to_string()))?
                );
            } else {
                print!("{}", report.render());
            }
        }
        Cmd::Init { .. } | Cmd::Serve { .. } | Cmd::Bench { .. } => unreachable!(),
    }
    Ok(())
}
