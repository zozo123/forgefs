#![no_main]

use forge_api::Forge;
use forge_cap::Cap;
use forge_core::validate_name;
use forge_types::{hex_encode, CasResult, ObjectId};
use libfuzzer_sys::fuzz_target;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const REF_A: &str = "heads/agents/fuzz/tar-a";
const REF_B: &str = "heads/agents/fuzz/tar-b";
const MAX_ENTRIES: usize = 24;
const MAX_DEPTH: usize = 4;

struct Fixture {
    forge: Forge,
    cap: Cap,
    scratch: PathBuf,
    seq: AtomicU64,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("forgefs-tar-fuzz-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let scratch = base.join("scratch");
        fs::create_dir_all(&scratch).expect("create fuzz scratch");
        let forge = Forge::init(&base.join("repo")).expect("initialize throwaway fuzz forge");
        let cap = forge.root_cap().expect("read throwaway root cap");
        Fixture {
            forge,
            cap,
            scratch,
            seq: AtomicU64::new(0),
        }
    })
}

/// Turn the fuzz input into a host directory. Anything the host itself would
/// reject is dropped rather than reported: only the ForgeFS round trip is
/// under test here, not `mkdir`.
fn materialize(root: &Path, data: &[u8]) -> usize {
    let Ok(text) = std::str::from_utf8(data) else {
        return 0;
    };
    let mut made = 0usize;
    for line in text.split('\n').take(MAX_ENTRIES) {
        let (path, body) = line.split_once('\u{1}').unwrap_or((line, ""));
        let exec = body.starts_with('\u{2}');
        let contents = body.strip_prefix('\u{2}').unwrap_or(body);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.is_empty() || parts.len() > MAX_DEPTH {
            continue;
        }
        if parts.iter().any(|p| validate_name(p).is_err()) {
            continue;
        }
        let target = parts.iter().fold(root.to_path_buf(), |acc, p| acc.join(p));
        if let Some(parent) = target.parent() {
            if fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        let Ok(mut f) = fs::File::create(&target) else {
            continue;
        };
        if f.write_all(contents.as_bytes()).is_err() {
            continue;
        }
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if exec { 0o755 } else { 0o644 };
            if fs::set_permissions(&target, fs::Permissions::from_mode(mode)).is_err() {
                continue;
            }
        }
        #[cfg(not(unix))]
        let _ = exec;
        made += 1;
    }
    made
}

fn commit_tree(forge: &Forge, result: &CasResult) -> Option<ObjectId> {
    let oid = match result {
        CasResult::Updated { oid, .. } | CasResult::Noop { oid, .. } => *oid,
        // A lost compare-and-swap is a legitimate outcome, not a round-trip
        // failure. It cannot happen in this single-threaded target, but the
        // target must not turn one into a false crash if that ever changes.
        CasResult::Forked { .. } => return None,
    };
    // A bare hex string parses as a REF name; an object id needs the `oid:`
    // spec prefix. Addressing the commit directly keeps the comparison exact
    // even if the ref moved.
    forge
        .peel_commit(&format!("oid:{}", hex_encode(oid.as_bytes())))
        .ok()
        .map(|(_, c)| c.tree)
}

// I10/I17: `export tar` followed by `import` must reproduce exactly the tree
// that was exported. Compare the content-addressed TREE, never the commit --
// commits embed a timestamp and a message, so two imports of identical bytes
// never have equal commit ids.
fuzz_target!(|data: &[u8]| {
    let fx = fixture();
    let n = fx.seq.fetch_add(1, Ordering::Relaxed);
    let src = fx.scratch.join(format!("src-{n}"));
    let out = fx.scratch.join(format!("out-{n}"));
    let tar_path = fx.scratch.join(format!("archive-{n}.tar"));

    let cleanup = || {
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_file(&tar_path);
    };

    if fs::create_dir_all(&src).is_err() {
        return;
    }
    if materialize(&src, data) == 0 {
        cleanup();
        return;
    }

    let Ok(first) = fx.forge.import_dir(&fx.cap, &src, REF_A) else {
        cleanup();
        return;
    };
    let Some(tree_in) = commit_tree(&fx.forge, &first) else {
        cleanup();
        return;
    };

    fx.forge
        .export_tar(&fx.cap, REF_A, &tar_path)
        .expect("exporting an imported tree must succeed");

    let file = fs::File::open(&tar_path).expect("exported archive is readable");
    let mut archive = tar::Archive::new(file);
    // The executable bit is part of tree-entry identity, so the extraction has
    // to carry it back out of the archive.
    archive.set_preserve_permissions(true);
    if fs::create_dir_all(&out).is_err() {
        cleanup();
        return;
    }
    archive
        .unpack(&out)
        .expect("a ForgeFS-written archive must unpack");

    let second = fx
        .forge
        .import_dir(&fx.cap, &out, REF_B)
        .expect("re-importing an extracted archive must succeed");
    let Some(tree_out) = commit_tree(&fx.forge, &second) else {
        cleanup();
        return;
    };

    assert_eq!(
        tree_in, tree_out,
        "tar export/import round trip lost or altered content"
    );
    cleanup();
});
