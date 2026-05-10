//! Pure business logic for the kanban orchestrator.
//!
//! No I/O, no DB handles — just the rules that govern the task
//! state machine and the worker-marker grammar. Lives in app-layer
//! because both the HTTP handlers and the orchestrator-tool
//! dispatcher consume the same functions; sharing them through a
//! `service` module keeps a single source of truth.

use super::model::TaskState;

/// Reasons a state transition can be refused. Surfaced to callers
/// so the HTTP layer can return a precise 4xx rather than a
/// generic "bad request".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// `from` cannot move to `to` under the legal-edges graph.
    IllegalEdge { from: TaskState, to: TaskState },
    /// Terminal states (`Done`, `Cancelled`) accept no outgoing
    /// transitions — the row is settled.
    Terminal(TaskState),
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::IllegalEdge { from, to } => {
                write!(f, "illegal transition {} → {}", from.as_str(), to.as_str())
            }
            TransitionError::Terminal(s) => {
                write!(f, "task in terminal state {} cannot transition", s.as_str())
            }
        }
    }
}

/// Decide whether `from → to` is a legal transition.
///
/// The graph is intentionally narrow — most edges only exist along
/// the happy path. NeedsHuman is reachable from every actionable
/// state (it's the explicit-escalation valve) and resolves back to
/// the previous logical step. Cancelled is reachable from any
/// non-terminal (the user can give up at any time).
pub fn validate_transition(from: TaskState, to: TaskState) -> Result<(), TransitionError> {
    if from.is_terminal() {
        return Err(TransitionError::Terminal(from));
    }
    if from == to {
        // No-op transitions are silently allowed (idempotent writes).
        return Ok(());
    }
    // Escape hatches reachable from any non-terminal state.
    if to == TaskState::NeedsHuman || to == TaskState::Cancelled {
        return Ok(());
    }
    // Resolution from NeedsHuman: the loop / UI resumes the task
    // by moving it back into the actionable graph. We accept any
    // non-terminal target — the orchestrator decides what makes
    // sense (e.g. "back to Code", "forward to AgentReview").
    if from == TaskState::NeedsHuman && !to.is_terminal() {
        return Ok(());
    }
    let legal = matches!(
        (from, to),
        // Triage path.
        (TaskState::Open, TaskState::Triage)
            | (TaskState::Triage, TaskState::Ready)
            // Coding path.
            | (TaskState::Ready, TaskState::Code)
            | (TaskState::Code, TaskState::AgentReview)
            | (TaskState::AgentReview, TaskState::HumanReview)
            // Reviewer wanted changes — back to Code.
            | (TaskState::AgentReview, TaskState::Code)
            | (TaskState::HumanReview, TaskState::Code)
            // Approval through merge.
            | (TaskState::HumanReview, TaskState::Merge)
            | (TaskState::Merge, TaskState::Done)
    );
    if legal {
        Ok(())
    } else {
        Err(TransitionError::IllegalEdge { from, to })
    }
}

/// Worker marker grammar — the structured suffix every worker is
/// prompted to end its reply with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerSignal {
    /// `<<<TASK_DONE: <summary>>>>` — worker finished its job.
    Done { summary: String },
    /// `<<<BLOCKED: <reason>>>>` — worker cannot proceed and is
    /// asking for human help.
    Blocked { reason: String },
    /// `<<<NEEDS_INPUT: <question>>>>` — worker has a clarification
    /// question that the **orchestrator** can answer (not human).
    /// Surfaces as a comment without flipping to NeedsHuman.
    NeedsInput { question: String },
    /// No marker found — worker is still mid-task or forgot to
    /// emit one. The orchestrator decides whether to wait or nudge.
    None,
}

/// Parse the last assistant message body for a marker.
///
/// Grammar:
/// - Marker must be the last non-empty line of the message.
/// - Open delimiter is `<<<KIND:`, close is `>>>` (three angle
///   brackets each side). Payload may contain `>>` but not `>>>`
///   (the parser scans for the literal three-bracket close).
/// - Payload is trimmed; empty payload → `None` (treat as missing).
/// - First match wins on the last line; we don't scan the rest of
///   the message for trailing markers — workers are instructed to
///   put exactly one marker as the last line.
///
/// Returns `WorkerSignal::None` for any message that doesn't end
/// with a recognized marker. That includes "marker on a line in
/// the middle of the message" — workers are required to put it at
/// the end so we can disambiguate from quoted text.
pub fn parse_worker_marker(body: &str) -> WorkerSignal {
    // Find the last non-empty trimmed line.
    let last_line = match body
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
    {
        Some(l) => l,
        None => return WorkerSignal::None,
    };
    // The marker takes the whole line, optionally surrounded by
    // whitespace (already trimmed). Recognize the three known
    // kinds.
    for (open, kind) in &[
        ("<<<TASK_DONE:", MarkerKind::Done),
        ("<<<BLOCKED:", MarkerKind::Blocked),
        ("<<<NEEDS_INPUT:", MarkerKind::NeedsInput),
    ] {
        if let Some(rest) = last_line.strip_prefix(open) {
            if let Some(payload) = rest.strip_suffix(">>>") {
                let payload = payload.trim();
                if payload.is_empty() {
                    return WorkerSignal::None;
                }
                let payload = payload.to_string();
                return match kind {
                    MarkerKind::Done => WorkerSignal::Done { summary: payload },
                    MarkerKind::Blocked => WorkerSignal::Blocked { reason: payload },
                    MarkerKind::NeedsInput => WorkerSignal::NeedsInput { question: payload },
                };
            }
        }
    }
    WorkerSignal::None
}

#[derive(Clone, Copy)]
enum MarkerKind {
    Done,
    Blocked,
    NeedsInput,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── transitions ─────────────────────────────────────────────

    #[test]
    fn happy_path_is_legal() {
        let happy = [
            (TaskState::Open, TaskState::Triage),
            (TaskState::Triage, TaskState::Ready),
            (TaskState::Ready, TaskState::Code),
            (TaskState::Code, TaskState::AgentReview),
            (TaskState::AgentReview, TaskState::HumanReview),
            (TaskState::HumanReview, TaskState::Merge),
            (TaskState::Merge, TaskState::Done),
        ];
        for (a, b) in happy {
            validate_transition(a, b).unwrap_or_else(|e| panic!("{a:?}→{b:?}: {e}"));
        }
    }

    #[test]
    fn review_back_to_code_is_legal() {
        validate_transition(TaskState::AgentReview, TaskState::Code).unwrap();
        validate_transition(TaskState::HumanReview, TaskState::Code).unwrap();
    }

    #[test]
    fn needs_human_escape_from_every_actionable_state() {
        let states = [
            TaskState::Open,
            TaskState::Triage,
            TaskState::Ready,
            TaskState::Code,
            TaskState::AgentReview,
            TaskState::HumanReview,
            TaskState::Merge,
        ];
        for s in states {
            validate_transition(s, TaskState::NeedsHuman).unwrap();
        }
    }

    #[test]
    fn needs_human_resumes_to_any_non_terminal() {
        validate_transition(TaskState::NeedsHuman, TaskState::Code).unwrap();
        validate_transition(TaskState::NeedsHuman, TaskState::AgentReview).unwrap();
        validate_transition(TaskState::NeedsHuman, TaskState::Triage).unwrap();
        // ...but not directly to terminal states without going through
        // the legal-edges graph (Cancelled is allowed as an escape).
        validate_transition(TaskState::NeedsHuman, TaskState::Cancelled).unwrap();
        assert!(validate_transition(TaskState::NeedsHuman, TaskState::Done).is_err());
    }

    #[test]
    fn cancelled_is_reachable_from_anywhere() {
        for s in [
            TaskState::Open,
            TaskState::Code,
            TaskState::HumanReview,
            TaskState::Merge,
        ] {
            validate_transition(s, TaskState::Cancelled).unwrap();
        }
    }

    #[test]
    fn terminal_blocks_outgoing() {
        for from in [TaskState::Done, TaskState::Cancelled] {
            for to in [
                TaskState::Open,
                TaskState::Code,
                TaskState::Done,
                TaskState::Cancelled,
            ] {
                match validate_transition(from, to) {
                    Err(TransitionError::Terminal(_)) => {}
                    other => panic!("{from:?}→{to:?} expected Terminal, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn illegal_jumps_are_rejected() {
        let bad = [
            (TaskState::Open, TaskState::Code),
            (TaskState::Ready, TaskState::Done),
            (TaskState::Code, TaskState::HumanReview),
            (TaskState::AgentReview, TaskState::Merge),
            (TaskState::HumanReview, TaskState::Done),
        ];
        for (a, b) in bad {
            assert!(
                matches!(
                    validate_transition(a, b),
                    Err(TransitionError::IllegalEdge { .. })
                ),
                "{a:?}→{b:?} should be illegal"
            );
        }
    }

    #[test]
    fn self_loops_are_no_ops() {
        validate_transition(TaskState::Code, TaskState::Code).unwrap();
    }

    // ── marker parser ────────────────────────────────────────────

    #[test]
    fn parse_done_marker_on_last_line() {
        let body = "Did the work.\n\n<<<TASK_DONE: fixed the typo>>>";
        assert_eq!(
            parse_worker_marker(body),
            WorkerSignal::Done {
                summary: "fixed the typo".into()
            }
        );
    }

    #[test]
    fn parse_blocked_marker() {
        let body = "Can't proceed.\n<<<BLOCKED: missing OPENAI_API_KEY>>>\n";
        assert_eq!(
            parse_worker_marker(body),
            WorkerSignal::Blocked {
                reason: "missing OPENAI_API_KEY".into()
            }
        );
    }

    #[test]
    fn parse_needs_input_marker() {
        let body = "<<<NEEDS_INPUT: which branch should I target?>>>";
        assert_eq!(
            parse_worker_marker(body),
            WorkerSignal::NeedsInput {
                question: "which branch should I target?".into()
            }
        );
    }

    #[test]
    fn marker_must_be_last_line() {
        let body = "<<<TASK_DONE: ok>>>\nstill working though";
        assert_eq!(parse_worker_marker(body), WorkerSignal::None);
    }

    #[test]
    fn trailing_whitespace_around_marker_ok() {
        let body = "all good\n   <<<TASK_DONE: yep>>>   \n\n";
        assert_eq!(
            parse_worker_marker(body),
            WorkerSignal::Done { summary: "yep".into() }
        );
    }

    #[test]
    fn empty_payload_is_treated_as_no_marker() {
        // A worker that emits the open/close but no payload is
        // basically signaling nothing useful; we ignore it so the
        // orchestrator can prompt for a real summary.
        let body = "<<<TASK_DONE: >>>";
        assert_eq!(parse_worker_marker(body), WorkerSignal::None);
    }

    #[test]
    fn no_marker_at_all_is_none() {
        let body = "Here is what I did.\nFile A: changed.\nFile B: deleted.";
        assert_eq!(parse_worker_marker(body), WorkerSignal::None);
    }

    #[test]
    fn empty_body_is_none() {
        assert_eq!(parse_worker_marker(""), WorkerSignal::None);
        assert_eq!(parse_worker_marker("\n\n\n"), WorkerSignal::None);
    }

    #[test]
    fn marker_with_inner_angle_brackets_in_payload() {
        // The payload may contain `<<<` or `>>` as long as the
        // close is the literal trailing `>>>`. We don't try to be
        // clever — last line, strip prefix, strip suffix, what's
        // left is the payload.
        let body = "<<<TASK_DONE: fixed > and >> chars>>>";
        assert_eq!(
            parse_worker_marker(body),
            WorkerSignal::Done {
                summary: "fixed > and >> chars".into()
            }
        );
    }
}
