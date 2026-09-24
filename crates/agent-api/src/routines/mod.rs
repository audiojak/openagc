//! Routines (spec §11): recurring agent tasks that file automated mail into
//! cadence labels. A routine is data; its prompt is generated from it.

mod prompt;

use serde::{Deserialize, Serialize};

pub use prompt::{PromptTarget, generate_prompt, prompt_fingerprint};

/// Where a routine runs (spec §11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runner {
    ClaudeCloud,
    ClaudeDesktop,
    ChatGptCloud,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// RFC 5545 recurrence rule, local time, e.g. `FREQ=HOURLY;BYMINUTE=44`.
    pub rrule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveAlone {
    pub human_threads: bool,
    pub replied_by_me: bool,
    pub starred: bool,
    pub spam_trash: bool,
    #[serde(default)]
    pub custom: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketColor {
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Brown,
    Gray,
}

impl BucketColor {
    /// The Claude Gmail connector's `colorPreset`.
    pub fn claude_preset(self) -> &'static str {
        match self {
            BucketColor::Red => "LABEL_COLOR_PRESET_RED",
            BucketColor::Orange => "LABEL_COLOR_PRESET_ORANGE",
            BucketColor::Yellow => "LABEL_COLOR_PRESET_YELLOW",
            BucketColor::Green => "LABEL_COLOR_PRESET_GREEN",
            BucketColor::Teal => "LABEL_COLOR_PRESET_TEAL",
            BucketColor::Blue => "LABEL_COLOR_PRESET_BLUE",
            BucketColor::Purple => "LABEL_COLOR_PRESET_PURPLE",
            BucketColor::Pink => "LABEL_COLOR_PRESET_PINK",
            BucketColor::Brown => "LABEL_COLOR_PRESET_BROWN",
            BucketColor::Gray => "LABEL_COLOR_PRESET_GRAY",
        }
    }

    /// A background from Gmail's label palette, for OpenAGC's own tools.
    pub fn gmail_hex(self) -> &'static str {
        match self {
            BucketColor::Red => "#fb4c2f",
            BucketColor::Orange => "#ffad47",
            BucketColor::Yellow => "#fad165",
            BucketColor::Green => "#16a766",
            BucketColor::Teal => "#43d692",
            BucketColor::Blue => "#4a86e8",
            BucketColor::Purple => "#a479e2",
            BucketColor::Pink => "#f691b3",
            BucketColor::Brown => "#8a1c0a",
            BucketColor::Gray => "#999999",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Stable within the routine; used by reports and run records.
    pub id: String,
    /// Under the parent label, e.g. `1-Daily`.
    pub label_name: String,
    pub color: BucketColor,
    pub cadence: Cadence,
    pub title: String,
    /// One paragraph: what belongs here.
    pub description: String,
    #[serde(default)]
    pub positive_examples: Vec<String>,
    /// "Not this — see …" cross-references.
    #[serde(default)]
    pub negative_examples: Vec<String>,
    #[serde(default)]
    pub priority_when_ambiguous: Option<String>,
    #[serde(default)]
    pub list_individually_in_report: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Unmatched {
    /// Leave automated mail that fits no bucket, and list it in the report.
    LeaveAndReport,
    ApplyLabel {
        label: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSpec {
    pub counts: bool,
    pub max_lines: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub primary_email: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub frequently_cc: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_threads_per_run: u32,
    pub get_thread_only_when_needed: bool,
}

/// What OpenAGC knows about the routine's cloud copy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudState {
    pub trigger_id: Option<String>,
    pub routine_url: Option<String>,
    pub environment_id: Option<String>,
    /// `prompt_fingerprint` of what was last published.
    pub published_fingerprint: Option<String>,
    pub published_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRef {
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub template: Option<TemplateRef>,
    pub runner: Runner,
    pub schedule: Schedule,
    /// For the local runner: which agent runs it.
    pub agent: Option<crate::ProviderId>,
    /// Gmail-style search for candidate threads.
    pub scope: String,
    pub parent_label: String,
    pub leave_alone: LeaveAlone,
    pub buckets: Vec<Bucket>,
    pub unmatched: Unmatched,
    pub report: ReportSpec,
    pub identity: Identity,
    pub limits: Limits,
    /// Set when the user edits the generated prompt by hand; generation
    /// then stops until they reset it.
    #[serde(default)]
    pub advanced_prompt: Option<String>,
    #[serde(default)]
    pub cloud: CloudState,
}

/// The bundled template file (spec §11.3).
#[derive(Debug, Clone, Deserialize)]
struct TemplateFile {
    template_id: String,
    template_version: u32,
    name: String,
    schedule: Schedule,
    scope: String,
    parent_label: String,
    leave_alone: LeaveAlone,
    buckets: Vec<Bucket>,
    unmatched: Unmatched,
    report: ReportSpec,
    limits: Limits,
}

pub const SORT_IMPORTANT_V1: &str = include_str!("../../templates/sort-important.v1.json");

impl Routine {
    /// A new routine from the shipped *Sort important mail* template, for
    /// this account.
    pub fn sort_important(id: impl Into<String>, identity: Identity, runner: Runner) -> Self {
        let t: TemplateFile = serde_json::from_str(SORT_IMPORTANT_V1).expect("the bundled template parses");
        Routine {
            id: id.into(),
            name: t.name,
            enabled: true,
            template: Some(TemplateRef { id: t.template_id, version: t.template_version }),
            runner,
            schedule: t.schedule,
            agent: (runner == Runner::Local).then_some(crate::ProviderId::ClaudeCode),
            scope: t.scope,
            parent_label: t.parent_label,
            leave_alone: t.leave_alone,
            buckets: t.buckets,
            unmatched: t.unmatched,
            report: t.report,
            identity,
            limits: t.limits,
            advanced_prompt: None,
            cloud: CloudState::default(),
        }
    }

    /// `Parent/Child` for a bucket.
    pub fn full_label(&self, bucket: &Bucket) -> String {
        format!("{}/{}", self.parent_label, bucket.label_name)
    }

    /// Problems that would make the routine misbehave, for the editor.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.name.trim().is_empty() {
            problems.push("The routine needs a name.".to_owned());
        }
        if self.buckets.is_empty() {
            problems.push("Add at least one bucket.".to_owned());
        }
        let mut seen = std::collections::HashSet::new();
        for b in &self.buckets {
            if b.label_name.trim().is_empty() {
                problems.push(format!("Bucket “{}” needs a label name.", b.title));
            }
            if !seen.insert(b.label_name.to_ascii_lowercase()) {
                problems.push(format!("Two buckets use the label {}.", b.label_name));
            }
        }
        if self.parent_label.trim().is_empty() || self.parent_label.contains('/') {
            problems.push("The parent label must be a single name without “/”.".to_owned());
        }
        if !(1..=500).contains(&self.limits.max_threads_per_run) {
            problems.push("A run can sort between 1 and 500 threads.".to_owned());
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Identity {
        Identity {
            primary_email: "me@example.com".into(),
            aliases: vec!["me@work.example".into()],
            frequently_cc: vec![],
        }
    }

    #[test]
    fn the_template_builds_a_valid_routine() {
        let r = Routine::sort_important("r1", me(), Runner::ClaudeCloud);
        assert_eq!(r.buckets.len(), 6);
        assert_eq!(r.full_label(&r.buckets[0]), "Marked Important/1-Daily");
        assert_eq!(r.buckets[5].color.claude_preset(), "LABEL_COLOR_PRESET_PURPLE");
        assert_eq!(r.template, Some(TemplateRef { id: "sort-important".into(), version: 1 }));
        assert!(r.validate().is_empty(), "{:?}", r.validate());
        assert_eq!(r.agent, None, "cloud routines use the vendor's agent");
        assert_eq!(Routine::sort_important("r2", me(), Runner::Local).agent, Some(crate::ProviderId::ClaudeCode));
    }

    #[test]
    fn the_template_has_no_personal_details() {
        let t = SORT_IMPORTANT_V1.to_ascii_lowercase();
        for word in ["actual", "john", "sprintreview", "autocommit", "@"] {
            assert!(!t.contains(word), "template mentions {word}");
        }
    }

    #[test]
    fn a_routine_round_trips_as_json_and_validates() {
        let mut r = Routine::sort_important("r1", me(), Runner::Local);
        let back: Routine = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        r.buckets[1].label_name = "1-daily".into();
        r.parent_label = "A/B".into();
        let problems = r.validate();
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
