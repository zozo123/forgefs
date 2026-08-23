use forge_cap::{attenuate_holder, mint_root, Op};

#[test]
fn comma_ref_not_is_equivalent_to_separate_exclusions() {
    let key = [7u8; 32];
    let root = mint_root(&key).unwrap();

    let separate = attenuate_holder(
        &root,
        vec!["ref!=main".into(), "ref!=tags/*".into()],
    )
    .unwrap();
    let combined = attenuate_holder(&root, vec!["ref!=main,tags/*".into()]).unwrap();

    for name in ["main", "tags/v1"] {
        assert!(separate.allows(Op::Read, Some(name), 0).is_err());
        assert!(
            combined.allows(Op::Read, Some(name), 0).is_err(),
            "comma-separated ref!= must not silently broaden authority for {name}"
        );
    }

    // The caveat must remain monotonic for unrelated names too: combining
    // exclusions may only preserve or shrink the holder's authority.
    assert_eq!(
        separate.allows(Op::Read, Some("heads/agents/a/x"), 0).is_ok(),
        combined.allows(Op::Read, Some("heads/agents/a/x"), 0).is_ok()
    );
}
