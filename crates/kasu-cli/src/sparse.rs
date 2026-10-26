use crate::{Result, validate_work_path};
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SparseRule {
    pub path: Vec<u8>,
    pub include: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SparseProfile {
    pub rules: Vec<SparseRule>,
}
pub fn prefix(parent: &[u8], child: &[u8]) -> bool {
    parent.is_empty()
        || parent == child
        || child
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with(b"/"))
}
impl SparseProfile {
    pub fn new(paths: Vec<Vec<u8>>) -> Result<Self> {
        let mut value = Self { rules: vec![] };
        if paths.is_empty() {
            value.set(vec![], true)?;
        } else {
            for path in paths {
                value.set(path, true)?;
            }
        }
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            if !rule.path.is_empty() {
                validate_work_path(&rule.path)?;
            }
        }
        Ok(())
    }
    pub fn set(&mut self, path: Vec<u8>, include: bool) -> Result<()> {
        if !path.is_empty() {
            validate_work_path(&path)?;
        }
        self.rules.retain(|rule| !prefix(&path, &rule.path));
        self.rules.push(SparseRule { path, include });
        self.rules.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }
    pub fn includes(&self, path: &[u8]) -> bool {
        self.rules
            .iter()
            .filter(|rule| prefix(&rule.path, path))
            .max_by_key(|rule| rule.path.len())
            .is_some_and(|rule| rule.include)
    }
    pub fn enters(&self, path: &[u8]) -> bool {
        self.includes(path)
            || self
                .rules
                .iter()
                .any(|rule| rule.include && prefix(path, &rule.path))
    }
}
