//! A LONG-LIVED, MULTI-BATCH workload for the power-loss harness (I4).
//!
//! Every `forge` CLI invocation is one process running one publish batch, so
//! a CLI workload can never exercise anything that lives in the process-local
//! durability caches (`durable_dirs`, `durable_oids`). Those caches are
//! exactly where a wrong proof can be recorded, and the crash suite -- CLI
//! processes plus `kill -9` -- is blind to them twice over: once because the
//! page cache survives the kill, and once because the cache never spans two
//! batches. The daemon (`forge serve`) and every library embedding DO run
//! many batches in one process, and this is that shape.
//!
//! The pattern is the one the product itself advises. A session holding two
//! read-write mounts with work staged under only one gets an I22 refusal from
//! `checkin` on the empty mount -- "check each mount in on its own
//! (checkin --mount <path>)" -- and the caller then does precisely that. The
//! refused checkin has already folded the other mount's overlay into a
//! publish batch, so it drops a batch that created object shard directories,
//! and the checkin that follows publishes a ref naming objects in those same
//! shards.
//!
//!   powerloss_multibatch <repo-dir> <ack-file> <iterations>
//!
//! Each `Updated` outcome appends `updated <ref> <oid>` to the ack file with
//! one `write(2)`, so the harness's interposer orders the promise into the
//! same stream as the writes that were supposed to make it durable.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use forge_api::Forge;
use forge_types::CasResult;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: powerloss_multibatch <repo-dir> <ack-file> <iterations>");
        std::process::exit(2);
    }
    let repo = Path::new(&args[1]);
    let ack_path = &args[2];
    let iterations: usize = args[3].parse().expect("iterations must be a number");

    // ONE process, ONE Forge, many batches: the whole point of this workload.
    let forge = Forge::open(repo).expect("open repository");
    let cap = forge.root_cap().expect("root capability");

    for i in 0..iterations {
        let shared = format!("shared-{i}");
        if forge.branch(&cap, "main", &shared).is_err() {
            continue;
        }
        let ns = match forge.session_open(&cap, "main") {
            Ok(ns) => ns,
            Err(_) => continue,
        };
        if forge
            .mount(&cap, &ns, "/s", &format!("ref:{shared}"), true)
            .is_err()
        {
            continue;
        }
        let body = format!("multibatch-{i}-{}", std::process::id());
        if forge
            .write(&cap, &ns, "/s/work.txt", body.as_bytes(), false)
            .is_err()
        {
            continue;
        }

        // Nothing is staged under `/`, so this refuses under I22 -- after it
        // has already folded `/s` into a publish batch it now drops.
        let _ = forge.checkin(&cap, &ns, "/", "refused");

        // The very checkin the refusal advises. It republishes objects the
        // dropped batch created shard directories for.
        // Only `Updated` is a promise; `Forked` and every refusal are not.
        if let Ok(CasResult::Updated { name, oid }) = forge.checkin(&cap, &ns, "/s", "published") {
            let line = format!("updated {name} {oid}\n");
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(ack_path)
                .expect("open ack file");
            // std::io::Write on a File is a write(2); the interposer sees it,
            // unlike a shell builtin's stdio flush.
            f.write_all(line.as_bytes()).expect("append ack");
        }
    }
}
