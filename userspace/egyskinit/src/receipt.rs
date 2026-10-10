//! Bounded loader/handoff errors; durable generic failure handling is in egysk-runtime.
use std::fmt;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Configuration,
    Storage,
    ModuleLoad,
    ModuleCheck,
    Handoff,
}
#[derive(Debug)]
pub struct Failure {
    pub stage: Stage,
    pub component: Option<String>,
    pub error: &'static str,
    pub detail: String,
}
fn bound(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
impl Failure {
    pub fn new(stage: Stage, error: &'static str, detail: impl AsRef<str>) -> Self {
        Self::at(stage, None, error, detail)
    }
    pub fn at(
        stage: Stage,
        component: Option<&str>,
        error: &'static str,
        detail: impl AsRef<str>,
    ) -> Self {
        Self {
            stage,
            component: component.map(|s| bound(s, 128)),
            error,
            detail: bound(detail.as_ref(), 1024),
        }
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {}: {}", self.stage, self.error, self.detail)
    }
}
impl std::error::Error for Failure {}
