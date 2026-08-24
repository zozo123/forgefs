#![no_main]

use forge_store::{validate_ref_kind, validate_ref_name};
use libfuzzer_sys::fuzz_target;

const KINDS: [&str; 7] = [
    "commit",
    "snapshot",
    "conflict",
    "tree",
    "blob",
    "contribution",
    "",
];

// Ref names are the mutable surface a peer can name directly, so their grammar
// is a trust boundary: it must fail closed on control characters, empty
// components, and `..` traversal, and it must never panic (I3/I5).
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (name, kind) = match text.split_once('\u{1}') {
        Some((n, k)) => (n, k),
        None => (text, "commit"),
    };

    if validate_ref_name(name).is_ok() {
        assert!(
            !name.is_empty() && name.len() <= 512,
            "accepted out-of-range ref name {name:?}"
        );
        assert!(
            !name.starts_with('/') && !name.ends_with('/'),
            "accepted ref name with a bare separator edge {name:?}"
        );
        assert!(
            !name
                .chars()
                .any(|c| c.is_control() || c == '\\' || c == ':'),
            "accepted ref name with a reserved character {name:?}"
        );
        assert!(
            !name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
            "accepted ref name with an empty or traversal component {name:?}"
        );
    }

    for candidate in KINDS.iter().copied().chain(std::iter::once(kind)) {
        if validate_ref_kind(name, candidate).is_ok() {
            // The kind check is a refinement of the name check; it can never be
            // the weaker of the two.
            assert!(
                validate_ref_name(name).is_ok(),
                "ref kind check accepted a name the name check rejects: {name:?}"
            );
            // Namespaces with a mandated object type never admit a second kind.
            let mandated = name == "main"
                || name.starts_with("heads/")
                || name.starts_with("forks/")
                || name.starts_with("conflicts/")
                || name.starts_with("tags/")
                || name.starts_with("inbox/");
            if mandated {
                let others = KINDS
                    .iter()
                    .filter(|k| **k != candidate)
                    .filter(|k| validate_ref_kind(name, k).is_ok())
                    .count();
                assert_eq!(
                    others, 0,
                    "typed ref namespace {name:?} accepted more than one kind"
                );
            }
        }
    }
});
