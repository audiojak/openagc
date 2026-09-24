//! Routine → prompt (spec §11.4). Filled in by oagc-bhm.

use super::{Routine, Runner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTarget {
    Runner(Runner),
}

pub fn generate_prompt(routine: &Routine, _target: PromptTarget) -> String {
    routine.advanced_prompt.clone().unwrap_or_default()
}

pub fn prompt_fingerprint(prompt: &str) -> String {
    format!("{:x}", prompt.len())
}
