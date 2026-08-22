from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one anchor, found {n}")
    return text.replace(old, new, 1)


# API module/export.
p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''mod bench;
mod export;
mod serve;

pub use bench::{''',
    '''mod bench;
mod export;
mod fsck;
mod serve;

pub use bench::{''',
    "api module",
)
s = replace_once(
    s,
    '''pub use serve::serve;
''',
    '''pub use fsck::{FsckFinding, FsckReport};
pub use serve::serve;
''',
    "api fsck export",
)
p.write_text(s)


# Metadata enumeration for integrity validation.
p = Path("crates/forge-store/src/meta.rs")
s = p.read_text()
anchor = '''    pub fn insert_namespace(
'''
method = '''    pub fn list_namespaces(&self) -> Result<Vec<NsRow>> {
        let conn = self.write.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, agent_id, pinned_oid, live_ref FROM namespaces ORDER BY id",
            )
            .map_err(map_sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(map_sql)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, agent_id, pinned_oid, live_ref) = row.map_err(map_sql)?;
            out.push(NsRow {
                id,
                agent_id,
                pinned_oid: pinned_oid.map(oid_from_blob).transpose()?,
                live_ref,
            });
        }
        Ok(out)
    }

'''
pos = s.find(anchor)
if pos < 0:
    raise SystemExit("insert_namespace anchor missing")
s = s[:pos] + method + s[pos:]
p.write_text(s)


# CLI surface.
p = Path("crates/forge-cli/src/main.rs")
s = p.read_text()
s = replace_once(
    s,
    '''    Show {
        spec: String,
    },
    /// Timed concurrent checkin / shared-ref stampede / merge+seal+verify.
    Bench {''',
    '''    Show {
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
    Bench {''',
    "cli command",
)
s = replace_once(
    s,
    '''        Cmd::Show { spec } => {
            println!("{}", f.show(cap, &spec)?);
        }
        Cmd::Init { .. } | Cmd::Serve { .. } | Cmd::Bench { .. } => unreachable!(),''',
    '''        Cmd::Show { spec } => {
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
        Cmd::Init { .. } | Cmd::Serve { .. } | Cmd::Bench { .. } => unreachable!(),''',
    "cli dispatch",
)
p.write_text(s)
