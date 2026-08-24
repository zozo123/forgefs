//! I13/I14 capability verification, attenuation, and namespace ownership.

use super::*;

impl Forge {
    pub fn load_cap(&self, token: &str) -> Result<Cap> {
        let cap = Cap::from_token(token.trim())?;
        verify(&self.hmac_key, &cap)?;
        Ok(cap)
    }

    pub fn root_cap(&self) -> Result<Cap> {
        let t = fs::read_to_string(self.root.join("keys/root.cap"))?;
        self.load_cap(t.trim())
    }

    pub fn integrator_cap(&self) -> Result<Cap> {
        let t = fs::read_to_string(self.root.join("keys/integrator.cap"))?;
        self.load_cap(t.trim())
    }

    pub(crate) fn check(&self, cap: &Cap, op: Op, r#ref: Option<&str>) -> Result<()> {
        verify(&self.hmac_key, cap)?;
        cap.allows(op, r#ref, now_ms())
    }

    pub(crate) fn require_ns(&self, cap: &Cap, ns: &str) -> Result<forge_store::NsRow> {
        let row = self.store.meta.get_namespace(ns)?;
        if row.agent_id == cap.agent_id() {
            Ok(row)
        } else {
            Err(Error::Denied(format!(
                "namespace {ns} is owned by {}, not {}",
                row.agent_id,
                cap.agent_id()
            )))
        }
    }

    pub(crate) fn check_spec_read(&self, cap: &Cap, spec: &str) -> Result<()> {
        match parse_spec(spec)? {
            Spec::Ref(n) => self.check(cap, Op::Read, Some(&n)),
            Spec::Oid(_) => {
                if cap.has_unrestricted_ref_scope() {
                    self.check(cap, Op::Read, None)
                } else {
                    Err(Error::Denied(
                        "ref-scoped caps cannot address raw object ids".into(),
                    ))
                }
            }
        }
    }

    pub fn grant(&self, cap: &Cap, extra: Vec<String>) -> Result<Cap> {
        self.check(cap, Op::Grant, None)?;
        attenuate(&self.hmac_key, cap, extra)
    }
}
