from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected one anchor, found {n}")
    return text.replace(old, new, 1)


def replace_block(text: str, start: str, end: str, new: str, label: str) -> str:
    a = text.find(start)
    if a < 0:
        raise SystemExit(f"{label}: start missing")
    b = text.find(end, a)
    if b < 0:
        raise SystemExit(f"{label}: end missing")
    return text[:a] + new + text[b:]


# API: mutable metadata stores public trust only, and it must match the local signer.
p = Path("crates/forge-api/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '        fs::write(root.join("keys/seal.pub"), pk)?;',
    '        write_public(root.join("keys/seal.pub"), &pk)?;',
    "durable seal.pub",
)
s = replace_once(
    s,
    "        store.meta.set_cap_root(&hmac_key, &pk)?;",
    "        store.meta.set_cap_root(&pk)?;",
    "public-only trust root",
)
s = replace_once(
    s,
    '''        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let store = Store::open(&root)?;
        let sk = SigningKey::from_bytes(&seal_seed);
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk: sk.verifying_key().to_bytes(),
            root,
        })''',
    '''        let hmac = read32(&root.join("keys/root.secret"))?;
        let seal_seed = read32(&root.join("keys/seal.ed25519"))?;
        let sk = SigningKey::from_bytes(&seal_seed);
        let seal_pk = sk.verifying_key().to_bytes();
        let store = Store::open(&root)?;
        let configured_pk = store.meta.get_seal_pub()?;
        if configured_pk != seal_pk.to_vec() {
            return Err(Error::Corrupt(
                "configured seal public key does not match local signing key".into(),
            ));
        }
        Ok(Self {
            store,
            hmac_key: hmac,
            seal_seed,
            seal_pk,
            root,
        })''',
    "open trust-root cross-check",
)
anchor = "fn sync_dir(path: &Path) -> Result<()> {"
pos = s.find(anchor)
if pos < 0:
    raise SystemExit("sync_dir helper missing")
s = s[:pos] + '''fn write_public(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

''' + s[pos:]
p.write_text(s)


# Metadata: keep legacy column for compatibility, but never retain the minting secret.
p = Path("crates/forge-store/src/meta.rs")
s = p.read_text()
s = replace_once(s, "  hmac_key BLOB NOT NULL,", "  hmac_key BLOB NOT NULL DEFAULT X'',", "cap_root schema")
s = replace_once(
    s,
    '''        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        Ok(Self {''',
    '''        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql)?;
        conn.execute(
            "UPDATE cap_root SET hmac_key=X'' WHERE length(hmac_key) != 0",
            [],
        )
        .map_err(map_sql)?;
        Ok(Self {''',
    "scrub legacy minting secret",
)
s = replace_block(
    s,
    "    pub fn set_cap_root",
    "    pub fn get_ref",
    '''    pub fn set_cap_root(&self, seal_pub: &[u8]) -> Result<()> {
        let conn = self.write.lock();
        conn.execute(
            "INSERT OR REPLACE INTO cap_root (id, hmac_key, seal_pub) VALUES (1, X'', ?1)",
            params![seal_pub],
        )
        .map_err(map_sql)?;
        Ok(())
    }

    pub fn get_seal_pub(&self) -> Result<Vec<u8>> {
        let conn = self.write.lock();
        conn.query_row("SELECT seal_pub FROM cap_root WHERE id=1", [], |r| r.get(0))
            .map_err(|_| Error::Corrupt("missing cap_root".into()))
    }

''',
    "cap_root API",
)
p.write_text(s)


# Provenance: an old edge expected to be a Tree must still be a Tree.
p = Path("crates/forge-store/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''                            ObjectType::Tree => Some(Tree::decode(&old_bytes)?),
                            ObjectType::Blob => None,
                            other => {''',
    '''                            ObjectType::Tree => Some(Tree::decode(&old_bytes)?),
                            other => {''',
    "strict previous tree edge",
)
p.write_text(s)


# Merge: decode/type corruption is not a semantic merge conflict.
p = Path("crates/forge-merge/src/lib.rs")
s = p.read_text()
s = replace_once(
    s,
    '''    let our_tree = store.get_tree(ours).ok();
    let their_tree = store.get_tree(theirs).ok();
    let base_tree = match base {
        Some(id) => store.get_tree(id).ok(),
        None => None,
    };
    if our_tree.is_none() || their_tree.is_none() {
        conflicts.push(ConflictPath {
            path: prefix.to_string(),
            a: Some(ours),
            b: Some(theirs),
            base,
        });
        return Ok(ours);
    }
    let a = our_tree.unwrap();
    let b = their_tree.unwrap();
    let g = base_tree.unwrap_or_else(Tree::default);''',
    '''    let a = store.get_tree(ours)?;
    let b = store.get_tree(theirs)?;
    let g = match base {
        Some(id) => store.get_tree(id)?,
        None => Tree::default(),
    };''',
    "merge corruption fail-closed",
)
p.write_text(s)
