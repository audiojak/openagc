//! Routine → prompt (spec §11.4). Deterministic for a given routine, target
//! and template version, and snapshot-tested.
//!
//! The order mirrors the proven production routine: unattended-run
//! preamble → purpose → identity and tools → Step 1 ensure labels → Step 2
//! find candidates (with the already-sorted re-apply rule) → Step 3 leave
//! alone → Step 4 sort, one paragraph per bucket → Step 5 report → hard
//! rules. Only the tool map differs between runners.

use std::fmt::Write;

use sha2::{Digest, Sha256};

use super::{Routine, Runner, Unmatched};

/// Who the prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTarget {
    Runner(Runner),
    /// The local runner, classifying without changing anything, for the
    /// editor's *Preview classification*.
    DryRun,
}

/// How a surface names the Gmail operations.
struct ToolMap {
    tools: &'static str,
    list_labels: &'static str,
    create_label: Option<&'static str>,
    search: &'static str,
    read: &'static str,
    label: &'static str,
    archive: &'static str,
    labels_by_id: bool,
    paging: &'static str,
    best_effort: bool,
}

const CLAUDE: ToolMap = ToolMap {
    tools: "the Gmail connector (tools: list_labels, search_threads, get_thread, create_label, label_thread, unlabel_thread)",
    list_labels: "list_labels",
    create_label: Some("create_label"),
    search: "search_threads",
    read: "get_thread",
    label: "label_thread",
    archive: "calling unlabel_thread with `INBOX`",
    labels_by_id: true,
    paging: "Use pageSize 50 and view THREAD_VIEW_MINIMAL. Paginate",
    best_effort: false,
};

const CHATGPT: ToolMap = ToolMap {
    tools: "the Gmail app (for example search_emails, read_email_thread and apply_labels_to_emails)",
    list_labels: "your label listing or search tools",
    create_label: None,
    search: "search_emails",
    read: "read_email_thread",
    label: "apply_labels_to_emails",
    archive: "removing the INBOX label with apply_labels_to_emails where that is supported (if it is not, leave the thread in the inbox and say so in the report)",
    labels_by_id: false,
    paging: "Page through the results",
    best_effort: true,
};

const LOCAL: ToolMap = ToolMap {
    tools: "OpenAGC's mail tools (mail_list_labels, mail_search, mail_get_thread, mail_create_label, mail_add_label, mail_archive)",
    list_labels: "mail_list_labels",
    create_label: Some("mail_create_label"),
    search: "mail_search",
    read: "mail_get_thread",
    label: "mail_add_label",
    archive: "calling mail_archive with its thread id",
    labels_by_id: false,
    paging: "Page with next_cursor",
    best_effort: false,
};

fn tool_map(target: PromptTarget) -> &'static ToolMap {
    match target {
        PromptTarget::Runner(Runner::ClaudeCloud | Runner::ClaudeDesktop) => &CLAUDE,
        PromptTarget::Runner(Runner::ChatGptCloud) => &CHATGPT,
        PromptTarget::Runner(Runner::Local) | PromptTarget::DryRun => &LOCAL,
    }
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

/// The prompt for `routine` on `target`. A hand-edited prompt wins, except
/// for a dry run, which always needs the generated no-changes version.
pub fn generate_prompt(r: &Routine, target: PromptTarget) -> String {
    if target != PromptTarget::DryRun
        && let Some(custom) = &r.advanced_prompt
    {
        return custom.clone();
    }
    let t = tool_map(target);
    let dry = target == PromptTarget::DryRun;
    let n = r.buckets.len();
    let cap = r.limits.max_threads_per_run;
    let labels: Vec<String> = r.buckets.iter().map(|b| format!("`{}`", r.full_label(b))).collect();
    let mut p = String::new();

    // Preamble.
    if dry {
        p.push_str(
            "This is a preview. Do not change anything: do not create labels, apply labels or archive. \
             Classify the candidate threads and reply with the result only.\n\n",
        );
    } else {
        let writes = match (t.create_label, t.labels_by_id) {
            (Some(create), true) => format!("{}, unlabel_thread, {create}", t.label),
            (Some(create), false) => format!("{create}, {}, mail_archive", t.label),
            (None, _) => t.label.to_owned(),
        };
        let _ = write!(
            p,
            "This is an automated, unattended run. The user is not present to answer questions — execute \
             autonomously, make reasonable choices, and note them in your output. Only take write actions \
             ({writes}) that this task explicitly asks for. Never send, reply, trash, delete, or mark as spam. \
             When in doubt, a report of what you found is the correct output.\n\n"
        );
    }

    // Purpose and identity.
    let _ = write!(
        p,
        "Sort the user's mail matching `{}` into cadence-based review labels under “{}”, so nothing important \
         is lost but the inbox stays clear.\n\n",
        r.scope, r.parent_label
    );
    let _ = write!(p, "Use {}. The user is {}.", t.tools, r.identity.primary_email);
    if !r.identity.aliases.is_empty() {
        let _ = write!(p, " They also receive mail at {}.", list(&r.identity.aliases));
    }
    if !r.identity.frequently_cc.is_empty() {
        let _ = write!(p, " They are frequently CC'd on mail to {}.", list(&r.identity.frequently_cc));
    }
    p.push('\n');
    if t.best_effort {
        p.push_str(
            "\nThe tool names above are a best guess. If a tool named here is not available, say which tools you \
             do have in the report and stop.\n",
        );
    }

    // Step 1.
    if !dry {
        let _ = write!(p, "\n## Step 1 — Ensure the labels exist\n\nCall {}. ", t.list_labels);
        match t.create_label {
            Some(create) => {
                let _ = writeln!(p, "If any of these {n} are missing, create them with {create}:\n");
                for b in &r.buckets {
                    let color = if t.labels_by_id {
                        format!("colorPreset {}", b.color.claude_preset())
                    } else {
                        format!("color {}", b.color.gmail_hex())
                    };
                    let _ = writeln!(p, "- `{}` — {color}", r.full_label(b));
                }
                if t.labels_by_id {
                    p.push_str(&format!(
                        "\nRecord each label's labelId — {} and unlabel_thread take IDs, not display names.\n",
                        t.label
                    ));
                }
            }
            None => {
                let _ = writeln!(
                    p,
                    "These {n} labels must exist: {}. If any is missing, report which ones and stop — do not \
                     sort anything.",
                    list(&labels)
                );
            }
        }
        p.push_str(
            "\nIf creating or applying a label fails with a permissions error, stop immediately and report that \
             the Gmail connection needs write access. Do not attempt workarounds.\n",
        );
    }

    // Step 2.
    let step = |k: u32| if dry { k - 1 } else { k };
    let _ = write!(p, "\n## Step {} — Find candidates\n\nSearch with {}: `{}`\n\n", step(2), t.search, r.scope);
    if t.labels_by_id {
        let _ = write!(
            p,
            "Then exclude anything already sorted: the parent “{}” label carries no messages of its own, so \
             append `-label:<id>` for each of the {n} label IDs from step 1. ",
            r.parent_label
        );
    }
    let _ = write!(
        p,
        "Threads that already carry one of the {n} labels ({}) were sorted on an earlier run. A thread can come \
         back because a newer message arrived: ",
        list(&labels)
    );
    if dry {
        p.push_str("leave those out of the preview.\n\n");
    } else {
        let _ = write!(
            p,
            "re-apply that same label with {} (so the new message gets it too) and archive it if it is back in \
             the inbox, but do not re-classify it or count it as new.\n\n",
            t.label
        );
    }
    let _ = write!(
        p,
        "{}, but stop after {cap} threads — if you hit that cap, say so rather than silently truncating.",
        t.paging
    );
    if r.limits.get_thread_only_when_needed {
        let _ = write!(
            p,
            " Classify from sender, subject and snippet. Only call {} when the snippet genuinely isn't enough to \
             decide — it is slow and most classifications are obvious from the sender.",
            t.read
        );
    }
    p.push('\n');

    // Step 3.
    let _ = write!(p, "\n## Step {} — Leave these alone\n\nDo NOT label and do NOT archive:\n\n", step(3));
    let la = &r.leave_alone;
    if la.human_threads {
        p.push_str(
            "- **Genuine person-to-person threads** — a real human writing to the user or a small group they are \
             part of, about actual work: introductions, deal threads, calendar invites from named people, event \
             logistics with named collaborators, replies to their own mail. When in doubt about whether a thread \
             is human correspondence, leave it in the inbox.\n",
        );
    }
    if la.replied_by_me {
        let mut me = vec![r.identity.primary_email.clone()];
        me.extend(r.identity.aliases.iter().cloned());
        let _ = writeln!(p, "- Any thread the user has already replied to (it contains a message from {}).", list(&me));
    }
    if la.starred {
        p.push_str("- Starred threads.\n");
    }
    if la.spam_trash {
        p.push_str("- Anything in Spam or Trash.\n");
    }
    for rule in &la.custom {
        let _ = writeln!(p, "- {}", rule.trim());
    }
    p.push_str(
        "\nThe principle behind these rules: wrongly deferring a real conversation is much worse than leaving one \
         extra message in the inbox.\n",
    );

    // Step 4.
    let _ = write!(p, "\n## Step {} — Sort the rest\n\n", step(4));
    if dry {
        p.push_str("Decide which one label each remaining thread would get:\n\n");
    } else {
        let _ = write!(
            p,
            "For each remaining thread, add exactly one label with {}, then archive it by {} (if the thread is \
             not in the inbox, just apply the label). Never trash, delete, or mark anything as spam. If a label or \
             archive call fails with a transient error (for example \"service unavailable\"), retry it once.\n\n",
            t.label, t.archive
        );
    }
    for b in &r.buckets {
        let _ = write!(p, "**`{}`** — {}", r.full_label(b), b.description.trim());
        if !b.positive_examples.is_empty() {
            let _ = write!(p, " For example: {}.", list(&b.positive_examples));
        }
        for not in &b.negative_examples {
            let _ = write!(p, " {}", not.trim());
        }
        if let Some(priority) = &b.priority_when_ambiguous {
            let _ = write!(p, " {}", priority.trim());
        }
        p.push_str("\n\n");
    }
    match &r.unmatched {
        Unmatched::LeaveAndReport => p.push_str(
            "Anything that fits none of these and is clearly automated: leave it untouched and mention it in the \
             report so the buckets can be adjusted over time.\n",
        ),
        Unmatched::ApplyLabel { label } => {
            let _ = writeln!(p, "Anything automated that fits none of these gets the label `{label}`.");
        }
    }

    // Step 5 / output.
    if dry {
        let ids: Vec<String> = r.buckets.iter().map(|b| format!("\"{}\"", b.id)).collect();
        let _ = write!(
            p,
            "\n## Result\n\nReply with only a JSON array, one object per candidate thread: \
             `{{\"thread_id\": \"…\", \"bucket\": one of [{}] or null to leave it, \"reason\": \"a few words\"}}`. \
             No other text.\n",
            ids.join(", ")
        );
        return p;
    }
    let _ = write!(p, "\n## Step 5 — Report\n\nEnd with a short report (at most {} lines):\n\n", r.report.max_lines);
    if r.report.counts {
        p.push_str("- Counts per label, plus how many threads were left in the inbox untouched.\n");
    }
    let listed: Vec<&super::Bucket> = r.buckets.iter().filter(|b| b.list_individually_in_report).collect();
    for b in &listed {
        let _ = writeln!(p, "- Every item in `{}`, listed individually with sender and subject.", b.label_name);
    }
    p.push_str("- Anything that didn't fit a bucket.\n- Any errors, or the thread cap being hit.\n");
    let quiet: Vec<String> =
        r.buckets.iter().filter(|b| !b.list_individually_in_report).map(|b| format!("`{}`", b.label_name)).collect();
    if !quiet.is_empty() {
        let _ = write!(p, "\nDo not list the {} items individually — counts are enough. ", list(&quiet));
    } else {
        p.push('\n');
    }
    p.push_str("If nothing new was found this run, say so in one line.\n");

    // Hard rules.
    let _ = write!(
        p,
        "\n## Rules that always apply\n\n- Never trash, delete, or mark anything as spam. Never send, reply or \
         forward.\n- Stop on permission errors and say so.\n- Stop after {cap} threads and say so.\n"
    );
    p
}

/// A short stable hash of a prompt, to tell whether a published copy is
/// out of date.
pub fn prompt_fingerprint(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The non-negotiable lines; the advanced editor warns if a hand-edited
/// prompt drops them (spec §11.8).
pub fn missing_safety_rules(prompt: &str) -> Vec<&'static str> {
    let lower = prompt.to_lowercase();
    let mut missing = Vec::new();
    if !(lower.contains("never trash") || lower.contains("never delete")) {
        missing.push("never trash or delete");
    }
    if !lower.contains("spam") {
        missing.push("never mark as spam");
    }
    if !lower.contains("never send") {
        missing.push("never send");
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routines::{Identity, Routine};

    fn routine(runner: Runner) -> Routine {
        let identity = Identity {
            primary_email: "me@example.com".into(),
            aliases: vec!["me@work.example".into(), "me@side.example".into()],
            frequently_cc: vec!["team@work.example".into()],
        };
        Routine::sort_important("r1", identity, runner)
    }

    #[test]
    fn claude_cloud_prompt() {
        insta::assert_snapshot!(
            "claude_cloud",
            generate_prompt(&routine(Runner::ClaudeCloud), PromptTarget::Runner(Runner::ClaudeCloud))
        );
    }

    #[test]
    fn chatgpt_prompt() {
        insta::assert_snapshot!(
            "chatgpt",
            generate_prompt(&routine(Runner::ChatGptCloud), PromptTarget::Runner(Runner::ChatGptCloud))
        );
    }

    #[test]
    fn local_prompt() {
        insta::assert_snapshot!("local", generate_prompt(&routine(Runner::Local), PromptTarget::Runner(Runner::Local)));
    }

    #[test]
    fn dry_run_prompt() {
        insta::assert_snapshot!("dry_run", generate_prompt(&routine(Runner::Local), PromptTarget::DryRun));
    }

    #[test]
    fn generation_is_deterministic_and_fingerprinted() {
        let a = generate_prompt(&routine(Runner::ClaudeCloud), PromptTarget::Runner(Runner::ClaudeCloud));
        let b = generate_prompt(&routine(Runner::ClaudeCloud), PromptTarget::Runner(Runner::ClaudeCloud));
        assert_eq!(a, b);
        assert_eq!(prompt_fingerprint(&a), prompt_fingerprint(&b));
        assert_eq!(prompt_fingerprint(&a).len(), 16);
        let mut edited = routine(Runner::ClaudeCloud);
        edited.buckets[0].description.push_str(" Also pager alerts.");
        assert_ne!(
            prompt_fingerprint(&generate_prompt(&edited, PromptTarget::Runner(Runner::ClaudeCloud))),
            prompt_fingerprint(&a)
        );
    }

    #[test]
    fn every_prompt_keeps_the_safety_rules_and_a_hand_edit_wins() {
        for runner in [Runner::ClaudeCloud, Runner::ClaudeDesktop, Runner::ChatGptCloud, Runner::Local] {
            let p = generate_prompt(&routine(runner), PromptTarget::Runner(runner));
            assert!(missing_safety_rules(&p).is_empty(), "{runner:?}");
            assert!(p.contains("Stop after 200 threads"));
        }
        let mut r = routine(Runner::Local);
        r.advanced_prompt = Some("Just label things.".into());
        assert_eq!(generate_prompt(&r, PromptTarget::Runner(Runner::Local)), "Just label things.");
        assert_eq!(missing_safety_rules("Just label things.").len(), 3);
        assert!(
            generate_prompt(&r, PromptTarget::DryRun).contains("Do not change anything"),
            "previews ignore hand edits"
        );
    }
}
