//! Per-session mount tables and overlay-aware path resolution.

use forge_core::split_path;
use forge_core::tree::TreeEntry;
use forge_store::{MountRow, OverlayRow, Store};
use forge_types::{EntryKind, Error, ObjectId, Result};

#[derive(Clone, Debug)]
pub struct Mount {
    pub path: String,
    pub spec: String,
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ro,
    Rw,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ro" => Ok(Mode::Ro),
            "rw" => Ok(Mode::Rw),
            other => Err(Error::Invalid(format!("mode {other}"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ro => "ro",
            Mode::Rw => "rw",
        }
    }
}

impl From<MountRow> for Mount {
    fn from(r: MountRow) -> Self {
        Mount {
            path: r.path,
            spec: r.spec,
            mode: Mode::parse(&r.mode).unwrap_or(Mode::Ro),
        }
    }
}

pub fn normalize_abs(path: &str) -> Result<String> {
    if path.is_empty() {
        return Ok("/".into());
    }
    let mut p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if p != "/" {
        split_path(&p)?;
    }
    Ok(p)
}

pub fn longest_mount<'a>(mounts: &'a [Mount], abs: &str) -> Result<&'a Mount> {
    let abs = normalize_abs(abs)?;
    let mut best: Option<&Mount> = None;
    let mut best_len = 0usize;
    for m in mounts {
        let mp = normalize_abs(&m.path)?;
        if abs == mp || (mp == "/") || abs.starts_with(&format!("{mp}/")) {
            if mp.len() >= best_len {
                best_len = mp.len();
                best = Some(m);
            }
        }
    }
    best.ok_or_else(|| Error::NotFound(format!("no mount for {abs}")))
}

pub fn rel_of(mount_path: &str, abs: &str) -> Result<String> {
    let mp = normalize_abs(mount_path)?;
    let abs = normalize_abs(abs)?;
    if mp == "/" {
        return Ok(abs.trim_start_matches('/').to_string());
    }
    if abs == mp {
        return Ok(String::new());
    }
    let prefix = format!("{mp}/");
    abs.strip_prefix(&prefix)
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Invalid(format!("{abs} not under {mp}")))
}

#[derive(Clone, Debug)]
pub enum Resolved {
    Tree(ObjectId),
    Blob { id: ObjectId, exec: bool },
}

pub fn overlay_map(rows: &[OverlayRow]) -> forge_core::Overlay {
    let mut m = forge_core::Overlay::new();
    for r in rows {
        m.insert(r.path.clone(), r.blob_oid.map(|id| (id, r.exec)));
    }
    m
}

/// Resolve a path through overlay then committed tree.
pub fn resolve(
    store: &Store,
    mounts: &[Mount],
    overlay: &[OverlayRow],
    root_oid: ObjectId, // tree oid of the selected mount
    abs: &str,
) -> Result<Resolved> {
    let m = longest_mount(mounts, abs)?;
    let rel = rel_of(&m.path, abs)?;
    let ov = overlay_map(overlay);
    if !rel.is_empty() {
        if let Some(op) = ov.get(&rel) {
            return match op {
                Some((id, exec)) => Ok(Resolved::Blob {
                    id: *id,
                    exec: *exec,
                }),
                None => Err(Error::NotFound(rel)),
            };
        }
    }
    if rel.is_empty() {
        return Ok(Resolved::Tree(root_oid));
    }
    let parts = split_path(&rel)?;
    let mut cur = root_oid;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let prefix: String = parts[..i].join("/");
        let path_so_far = if prefix.is_empty() {
            part.clone()
        } else {
            format!("{prefix}/{part}")
        };
        if let Some(op) = ov.get(&path_so_far) {
            match op {
                None => return Err(Error::NotFound(path_so_far)),
                Some((id, exec)) if last => {
                    return Ok(Resolved::Blob {
                        id: *id,
                        exec: *exec,
                    })
                }
                Some(_) => return Err(Error::Invalid("overlay file in middle of path".into())),
            }
        }
        let tree = store.get_tree(cur)?;
        let ent = tree
            .get(part)
            .ok_or_else(|| Error::NotFound(path_so_far.clone()))?;
        if last {
            return match ent.kind {
                EntryKind::Blob => Ok(Resolved::Blob {
                    id: ent.id,
                    exec: ent.exec,
                }),
                EntryKind::Tree => Ok(Resolved::Tree(ent.id)),
            };
        }
        if ent.kind != EntryKind::Tree {
            return Err(Error::NotFound(path_so_far));
        }
        cur = ent.id;
    }
    Ok(Resolved::Tree(cur))
}

pub fn ls(
    store: &Store,
    overlay: &[OverlayRow],
    tree_oid: ObjectId,
    rel: &str,
) -> Result<Vec<TreeEntry>> {
    let ov = overlay_map(overlay);
    let mut map = if rel.is_empty() {
        store.get_tree(tree_oid)?.as_map()
    } else {
        // walk to dir
        let parts = split_path(rel)?;
        let mut cur = tree_oid;
        for p in parts {
            let t = store.get_tree(cur)?;
            let e = t.get(&p).ok_or_else(|| Error::NotFound(p.clone()))?;
            if e.kind != EntryKind::Tree {
                return Err(Error::Invalid("ls on blob".into()));
            }
            cur = e.id;
        }
        store.get_tree(cur)?.as_map()
    };
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for (path, op) in &ov {
        let rest = if rel.is_empty() {
            path.as_str()
        } else if let Some(r) = path.strip_prefix(&prefix) {
            r
        } else if path == rel {
            continue;
        } else {
            continue;
        };
        if rest.contains('/') {
            let first = rest.split('/').next().unwrap();
            if !map.contains_key(first) {
                map.insert(
                    first.to_string(),
                    TreeEntry {
                        name: first.to_string(),
                        kind: EntryKind::Tree,
                        id: ObjectId::ZERO,
                        exec: false,
                    },
                );
            }
            continue;
        }
        match op {
            Some((id, exec)) => {
                map.insert(
                    rest.to_string(),
                    TreeEntry {
                        name: rest.to_string(),
                        kind: EntryKind::Blob,
                        id: *id,
                        exec: *exec,
                    },
                );
            }
            None => {
                map.remove(rest);
            }
        }
    }
    let mut v: Vec<_> = map.into_values().collect();
    v.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    Ok(v)
}

pub fn parse_spec(spec: &str) -> Result<Spec> {
    if let Some(r) = spec.strip_prefix("ref:") {
        Ok(Spec::Ref(r.to_string()))
    } else if let Some(h) = spec.strip_prefix("oid:") {
        Ok(Spec::Oid(ObjectId::from_hex(h)?))
    } else {
        Ok(Spec::Ref(spec.to_string()))
    }
}

#[derive(Clone, Debug)]
pub enum Spec {
    Ref(String),
    Oid(ObjectId),
}

pub fn dirent_kind(e: &TreeEntry) -> &'static str {
    match e.kind {
        EntryKind::Blob => "blob",
        EntryKind::Tree => "tree",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_and_rel() {
        let mounts = vec![
            Mount {
                path: "/".into(),
                spec: "ref:heads/a".into(),
                mode: Mode::Rw,
            },
            Mount {
                path: "/main".into(),
                spec: "ref:main".into(),
                mode: Mode::Ro,
            },
        ];
        let m = longest_mount(&mounts, "/main/src/a.rs").unwrap();
        assert_eq!(m.path, "/main");
        assert_eq!(rel_of("/main", "/main/src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(rel_of("/", "/foo").unwrap(), "foo");
    }
}
