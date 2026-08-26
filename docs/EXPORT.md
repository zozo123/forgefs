# Export portability

ForgeFS export is an archive boundary, not a host-filesystem materialization boundary.

`forge export <spec> -o <file.tar>` writes the exact UTF-8 tree-entry names into a tar archive. ForgeFS does not normalize, case-fold, or otherwise rewrite those names. This preserves I16: two different UTF-8 byte strings remain different repository names and different archive member names.

## Collisions are refused before the archive is written

Some filesystems cannot materialize every ForgeFS tree faithfully. In particular, case-insensitive filesystems can collapse names such as `Foo` and `foo`, and normalizing filesystems can collapse the NFC and NFD spellings of one visual name.

By default, export recursively computes a portability fold for every sibling name and refuses an archive when two entries would collide under case folding or Unicode NFC normalization. The diagnostic includes the original names and bytes, and a refused export leaves no partial destination. This check is independent of the filesystem on which ForgeFS itself is running.

`--allow-name-collisions` is the explicit escape hatch. It tells ForgeFS to produce the faithful archive even though extraction on a case-insensitive or normalizing target may overwrite one sibling. The archive still contains both exact member names; the requested risk begins when another tool extracts it.

## A single name can still be changed by an external extractor

The collision check prevents predictable two-name clobbering, but ForgeFS cannot control a third-party extractor. A tree can contain one non-normalized name with no sibling collision; the tar preserves its exact bytes, while an extractor or target filesystem may normalize that single name during materialization. The extracted path is then a different name under I16 even though the archive is correct.

There is no post-export filesystem scan ForgeFS can perform today: the product writes the tar file and does not perform extraction. Therefore:

- treat the tar member names as the authoritative exported representation;
- do not assume an extracted directory round-trips byte-for-byte on a normalizing filesystem;
- do not use `--allow-name-collisions` unless the destination's naming semantics are understood;
- if a future native `export-directory` adapter materializes files itself, it must re-read the materialized names and refuse any byte change rather than silently normalizing them.

Executable evidence lives in `crates/forge-api/tests/export_name_collisions.rs`: case collisions, NFC/NFD collisions, nested collisions, no-partial-artifact behavior, the explicit opt-out, and an ordinary export/extract/re-import round trip are all pinned there.
