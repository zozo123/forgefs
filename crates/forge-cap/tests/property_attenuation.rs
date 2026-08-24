//! I13 as executable algebra: Authority(c+d) is a subset of Authority(c).
//!
//! A holder attenuates with `attenuate_holder`, using only the current
//! signature. Whatever caveats they append, the set of (op, ref, clock)
//! triples the resulting capability authorizes can only shrink, and the
//! attenuated token must still verify under the same root secret.
//!
//! Deterministic and dependency-free: a SplitMix64 seed drives generation, so
//! a failure names the seed that produced it.

use forge_cap::{attenuate_holder, mint, verify, Cap, Op};

const ROOT: &[u8] = b"forgefs property-test root secret";
const CASES: u64 = 600;
const CHAIN: usize = 3;

const OPS: [Op; 6] = [
    Op::Read,
    Op::Write,
    Op::Branch,
    Op::Merge,
    Op::Grant,
    Op::Seal,
];

/// The probe grid. Every (op, ref, clock) triple here is one element of the
/// authority set being compared across an attenuation step.
const REFS: [Option<&str>; 8] = [
    None,
    Some("main"),
    Some("heads/agents/a/1"),
    Some("heads/agents/b/2"),
    Some("forks/a/1"),
    Some("tags/v1"),
    Some("conflicts/c1"),
    Some("heads/agents/a"),
];
const CLOCKS: [u64; 5] = [0, 500, 1_000, 1_001, u64::MAX];

/// Caveat vocabulary, drawn from what `forge grant` actually issues.
const CAVEATS: [&str; 14] = [
    "ops=read",
    "ops=read,write",
    "ops=read,write,branch",
    "ops=read,merge,seal,grant",
    "ops=read,write,branch,merge,grant,seal",
    "ref=main",
    "ref=heads/agents/a/*",
    "ref=main,heads/agents/*",
    "ref!=heads/agents/b*",
    "ref!=main",
    "allow=write:heads/agents/a/*",
    "allow=merge:main",
    "time<=1000",
    "agent=a",
];

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    fn caveats(&mut self, max: usize) -> Vec<String> {
        (0..self.below(max))
            .map(|_| CAVEATS[self.below(CAVEATS.len())].to_string())
            .collect()
    }
}

/// The reachable (op, ref, clock) set, as a bitmap over the probe grid.
fn authority(cap: &Cap) -> Vec<bool> {
    let mut out = Vec::with_capacity(OPS.len() * REFS.len() * CLOCKS.len());
    for op in OPS {
        for r in REFS {
            for now in CLOCKS {
                out.push(cap.allows(op, r, now).is_ok());
            }
        }
    }
    out
}

fn describe(index: usize) -> String {
    let clocks = CLOCKS.len();
    let refs = REFS.len();
    let now = CLOCKS[index % clocks];
    let r = REFS[(index / clocks) % refs];
    let op = OPS[index / (clocks * refs)];
    format!("op={} ref={:?} now_ms={}", op.as_str(), r, now)
}

/// Every capability ForgeFS mints carries an `ops=` caveat; one without it
/// authorizes nothing at all, so the generated roots below always start with
/// one. `no_ops_caveat_authorizes_nothing` pins that boundary separately.
fn base_cap(rng: &mut Rng) -> Option<Cap> {
    let mut caveats = vec![CAVEATS[rng.below(5)].to_string()];
    caveats.extend(rng.caveats(4));
    mint(ROOT, "forge", "property", caveats).ok()
}

#[test]
fn holder_attenuation_only_shrinks_authority_i13() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_cafe_0000);
        let Some(mut cap) = base_cap(&mut rng) else {
            continue;
        };
        verify(ROOT, &cap).expect("a freshly minted capability verifies");

        for step in 0..CHAIN {
            let before = authority(&cap);
            let extra = rng.caveats(3);
            let Ok(child) = attenuate_holder(&cap, extra.clone()) else {
                continue; // unparseable or oversized caveats are a rejection
            };

            // Attenuation is root-secret-free but must stay verifiable.
            verify(ROOT, &child).unwrap_or_else(|e| {
                panic!("seed {seed} step {step}: attenuated capability stopped verifying: {e:?}")
            });

            let after = authority(&child);
            for (i, (parent_allows, child_allows)) in before.iter().zip(after.iter()).enumerate() {
                if *child_allows {
                    assert!(
                        *parent_allows,
                        "seed {seed} step {step}: attenuation GRANTED authority it did not have: \
                         {} was denied by {:?} but allowed after appending {:?} (I13)",
                        describe(i),
                        cap.caveats,
                        extra
                    );
                }
            }
            cap = child;
        }
    }
}

#[test]
fn attenuation_is_transitive_and_monotone_i13() {
    for seed in 0..CASES {
        let mut rng = Rng(seed ^ 0x_5eed_0000);
        let Some(root) = base_cap(&mut rng) else {
            continue;
        };
        let first = rng.caveats(3);
        let second = rng.caveats(3);

        let Ok(a) = attenuate_holder(&root, first.clone()) else {
            continue;
        };
        let Ok(b) = attenuate_holder(&a, second.clone()) else {
            continue;
        };
        let mut both = first;
        both.extend(second);
        let Ok(direct) = attenuate_holder(&root, both) else {
            continue;
        };

        assert_eq!(
            b.to_token(),
            direct.to_token(),
            "seed {seed}: a two-step attenuation chain diverged from the one-step chain (I13)"
        );
        let (ra, rb) = (authority(&root), authority(&b));
        for (i, (parent_allows, child_allows)) in ra.iter().zip(rb.iter()).enumerate() {
            if *child_allows {
                assert!(
                    *parent_allows,
                    "seed {seed}: a chain of attenuations grew authority at {} (I13)",
                    describe(i)
                );
            }
        }
    }
}

#[test]
fn a_capability_with_no_ops_caveat_authorizes_nothing_i13() {
    // Fail closed: authority comes only from a signed `ops=` caveat. This is the
    // floor the monotonicity property above is measured against.
    let cap = mint(ROOT, "forge", "empty", vec![]).expect("mint with no caveats");
    assert!(
        authority(&cap).iter().all(|allowed| !allowed),
        "a capability carrying no ops caveat authorized something"
    );
}
