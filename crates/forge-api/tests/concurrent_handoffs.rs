//! #71: concurrent independent handoffs must not contend on one global ref.
//!
//! Each recipient gets its own inbox/<recv>/<task> leaf. At 1/32/256/1024
//! concurrent publishers every push must Update (never Fork against ZERO on a
//! fresh leaf), and fsck must stay clean.

use forge_api::Forge;
use forge_types::CasResult;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use tempfile::tempdir;

fn run_handoffs(n: usize, workers: usize) {
    let d = tempdir().unwrap();
    let forge = Arc::new(Forge::init(d.path()).unwrap());
    let root = forge.root_cap().unwrap();
    forge.seal(&root, "main", "handoff-base").unwrap();

    let next = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(workers.max(1).min(n.max(1)));
    for _ in 0..workers.max(1).min(n.max(1)) {
        let forge = forge.clone();
        let root = root.clone();
        let next = next.clone();
        handles.push(thread::spawn(move || {
            let mut updated = 0usize;
            let mut forked = 0usize;
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let sender = forge
                    .grant(
                        &root,
                        vec![
                            "ops=read,write".into(),
                            format!("agent=sender{i}"),
                            format!("ref=tags/handoff-base,inbox/recv{i}/*"),
                        ],
                    )
                    .unwrap();
                let result = forge
                    .inbox_push(
                        &sender,
                        &format!("recv{i}"),
                        "tags/handoff-base",
                        Some(&format!("task{i}")),
                    )
                    .unwrap();
                match result {
                    CasResult::Updated { name, .. } => {
                        assert_eq!(name, format!("inbox/recv{i}/task{i}"));
                        updated += 1;
                    }
                    CasResult::Forked { .. } => forked += 1,
                    other => panic!("unexpected handoff outcome: {other:?}"),
                }
            }
            (updated, forked)
        }));
    }

    let mut updated = 0usize;
    let mut forked = 0usize;
    for handle in handles {
        let (u, f) = handle.join().unwrap();
        updated += u;
        forked += f;
    }
    assert_eq!(updated, n, "every fresh task leaf must Update");
    assert_eq!(forked, 0, "independent recipients must not fork");
    let report = forge.fsck(&root, true).unwrap();
    assert!(
        report.findings.is_empty(),
        "fsck dirty after handoffs: {:?}",
        report.findings
    );
}

#[test]
fn concurrent_handoffs_scale_1_32_256_1024() {
    for &(n, workers) in &[(1, 1), (32, 8), (256, 32), (1024, 64)] {
        run_handoffs(n, workers);
    }
}
