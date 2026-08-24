use forge_api::Forge;
use forge_cap::Cap;
use std::path::Path;
use tempfile::{tempdir, TempDir};

pub struct Fixture {
    dir: TempDir,
    pub forge: Forge,
    pub root: Cap,
    pub integrator: Cap,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempdir().expect("fixture tempdir");
        let forge = Forge::init(dir.path()).expect("fixture init");
        let root = forge.root_cap().expect("fixture root capability");
        let integrator = forge
            .integrator_cap()
            .expect("fixture integrator capability");
        Self {
            dir,
            forge,
            root,
            integrator,
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn agent(&self, id: &str) -> Cap {
        self.forge
            .grant(
                &self.root,
                vec![
                    "ops=read,write,branch".into(),
                    format!("agent={id}"),
                    format!("ref=main,heads/agents/{id}/*"),
                ],
            )
            .expect("fixture agent capability")
    }

    pub fn session(&self, cap: &Cap, from: &str) -> String {
        self.forge.session_open(cap, from).expect("fixture session")
    }
}
