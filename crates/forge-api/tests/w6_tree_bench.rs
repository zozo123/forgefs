//! W6 (#24): large-tree walk/update harness — identity and reporting at CI scale.

use forge_api::{run_tree_bench, w6_path, w6_seed_payload, w6_update_payload, TreeBenchReport};
use tempfile::tempdir;

const CI_ENTRIES: usize = 1000;

fn assert_report_shape(report: &TreeBenchReport, entries: usize) {
    assert_eq!(report.entries, entries);
    assert_eq!(report.lookup.0.n, entries);
    assert_eq!(report.update.0.n, entries);
    assert!(report.fsck_ok);
    assert!(
        !report.export_blake3.is_empty(),
        "export digest must be recorded"
    );
    let rendered = report.render();
    assert!(rendered.contains("ForgeFS W6 tree walk/update"));
    assert!(rendered.contains("lookup           n="));
    assert!(rendered.contains("update           n="));
    assert!(rendered.contains("export identity  blake3="));
}

#[test]
fn w6_tree_bench_proves_payload_identity_and_reports_scaling() {
    let d = tempdir().unwrap();
    let report = run_tree_bench(d.path(), CI_ENTRIES).expect("W6 harness");
    assert_report_shape(&report, CI_ENTRIES);
}

#[test]
fn w6_export_is_byte_for_byte_identical_across_fresh_repositories() {
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    let ra = run_tree_bench(a.path(), CI_ENTRIES).unwrap();
    let rb = run_tree_bench(b.path(), CI_ENTRIES).unwrap();
    assert_eq!(
        ra.export_blake3, rb.export_blake3,
        "same entry count must yield the same exported tree bytes"
    );
}

#[test]
fn w6_paths_shard_directories_under_the_tree_limit() {
    assert_eq!(w6_path(0), "/d0000/f0000.txt");
    assert_eq!(w6_path(999), "/d0000/f0999.txt");
    assert_eq!(w6_path(1000), "/d0001/f0000.txt");
    assert_ne!(w6_seed_payload(1), w6_update_payload(1));
}
