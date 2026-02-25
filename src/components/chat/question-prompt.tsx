import * as React from "react";
import type { UserInputAnswer, UserInputQuestion } from "@/lib/types";

interface QuestionPromptProps {
  questions: UserInputQuestion[];
  /** Send answers back to the daemon. The component clears its own state
   *  via the parent unmounting it (chat-view sets pendingQuestion to null
   *  before calling onSubmit). */
  onSubmit: (answers: UserInputAnswer[]) => void;
  onCancel: () => void;
}

// Per-question working state. For single-select questions, optionIds has
// at most one entry. For multi-select, it can have many. `freeform` holds
// the optional text input when allowFreeform is true.
type Draft = {
  optionIds: string[];
  freeform: string;
};

function emptyDraft(): Draft {
  return { optionIds: [], freeform: "" };
}

function QuestionPromptInner({
  questions,
  onSubmit,
  onCancel,
}: QuestionPromptProps) {
  const [drafts, setDrafts] = React.useState<Draft[]>(() =>
    questions.map(() => emptyDraft()),
  );
  // Index of the question currently shown. Walks one-at-a-time via the
  // Next button; "Submit" only appears on the final question.
  const [cursor, setCursor] = React.useState(0);
  // Reset back to the first question if a fresh batch arrives — handles
  // the case where the same prompt is unmounted/remounted with new
  // questions in the same chat-view session.
  React.useEffect(() => {
    setCursor(0);
    setDrafts(questions.map(() => emptyDraft()));
  }, [questions]);

  function setDraft(idx: number, next: Draft) {
    setDrafts((prev) => prev.map((d, i) => (i === idx ? next : d)));
  }

  function toggleOption(idx: number, optionId: string, multi: boolean) {
    const current = drafts[idx];
    if (multi) {
      const has = current.optionIds.includes(optionId);
      setDraft(idx, {
        ...current,
        optionIds: has
          ? current.optionIds.filter((o) => o !== optionId)
          : [...current.optionIds, optionId],
      });
    } else {
      setDraft(idx, { ...current, optionIds: [optionId] });
    }
  }

  // A question is considered answered when at least one option is picked
  // OR (if allowFreeform) the freeform text is non-empty.
  function isAnswered(q: UserInputQuestion, d: Draft): boolean {
    if (d.optionIds.length > 0) return true;
    if (q.allowFreeform && d.freeform.trim().length > 0) return true;
    return false;
  }

  const totalQuestions = questions.length;
  const isLast = cursor === totalQuestions - 1;
  const isFirst = cursor === 0;
  const currentQuestion = questions[cursor];
  const currentDraft = drafts[cursor] ?? emptyDraft();
  const currentAnswered = isAnswered(currentQuestion, currentDraft);

  function handleNext() {
    if (!currentAnswered) return;
    setCursor((c) => Math.min(c + 1, totalQuestions - 1));
  }

  function handleBack() {
    setCursor((c) => Math.max(c - 1, 0));
  }

  function handleSubmit() {
    if (!currentAnswered) return;
    // The model expects an answer for every question, even if the user
    // didn't visit one (which shouldn't happen via the Next flow but
    // guard anyway). Default to empty answer for any unanswered slot.
    const answers: UserInputAnswer[] = questions.map((q, i) => {
      const d = drafts[i];
      const labels = d.optionIds
        .map((oid) => q.options.find((o) => o.id === oid)?.label)
        .filter((l): l is string => Boolean(l));
      const freeform = d.freeform.trim();
      const answer = freeform.length > 0 ? freeform : labels.join(", ");
      return {
        questionId: q.id,
        optionIds: d.optionIds,
        answer,
      };
    });
    onSubmit(answers);
  }

  return (
    <div className="shrink-0 border-t border-sky-500/40 bg-sky-500/5 px-3 py-3">
      <div className="mb-3 flex items-center justify-between gap-2">
        <div className="text-sm font-medium">Questions from the agent</div>
        {totalQuestions > 1 && (
          <div className="text-[11px] text-muted-foreground">
            Question {cursor + 1} of {totalQuestions}
          </div>
        )}
      </div>
      <QuestionBlock
        question={currentQuestion}
        draft={currentDraft}
        onToggleOption={(optionId) =>
          toggleOption(cursor, optionId, currentQuestion.multiSelect)
        }
        onFreeformChange={(text) =>
          setDraft(cursor, { ...currentDraft, freeform: text })
        }
      />
      <div className="mt-3 flex items-center justify-end gap-2">
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-input bg-background px-3 py-1.5 text-xs font-medium hover:bg-accent"
        >
          Skip
        </button>
        {!isFirst && (
          <button
            type="button"
            onClick={handleBack}
            className="rounded-md border border-input bg-background px-3 py-1.5 text-xs font-medium hover:bg-accent"
          >
            Back
          </button>
        )}
        {isLast ? (
          <button
            type="button"
            onClick={handleSubmit}
            disabled={!currentAnswered}
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            Submit
          </button>
        ) : (
          <button
            type="button"
            onClick={handleNext}
            disabled={!currentAnswered}
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            Next
          </button>
        )}
      </div>
    </div>
  );
}

function QuestionBlock({
  question,
  draft,
  onToggleOption,
  onFreeformChange,
}: {
  question: UserInputQuestion;
  draft: Draft;
  onToggleOption: (optionId: string) => void;
  onFreeformChange: (text: string) => void;
}) {
  const inputType = question.multiSelect ? "checkbox" : "radio";
  return (
    <div className="space-y-2">
      {question.header && (
        <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
          {question.header}
        </div>
      )}
      <div className="text-sm">{question.text}</div>
      <div className="space-y-1.5">
        {question.options.map((opt) => {
          const checked = draft.optionIds.includes(opt.id);
          return (
            <label
              key={opt.id}
              className={
                "flex cursor-pointer items-start gap-2 rounded-md border px-2 py-1.5 text-xs hover:bg-accent " +
                (checked
                  ? "border-primary bg-primary/5"
                  : "border-input bg-background")
              }
            >
              <input
                type={inputType}
                name={question.id}
                checked={checked}
                onChange={() => onToggleOption(opt.id)}
                className="mt-0.5"
              />
              <div className="min-w-0 flex-1">
                <div className="font-medium">{opt.label}</div>
                {opt.description && (
                  <div className="text-muted-foreground">{opt.description}</div>
                )}
              </div>
            </label>
          );
        })}
      </div>
      {question.allowFreeform && (
        <input
          type={question.isSecret ? "password" : "text"}
          value={draft.freeform}
          onChange={(e) => onFreeformChange(e.target.value)}
          placeholder="Or type your answer…"
          className="w-full rounded-md border border-input bg-background px-2 py-1.5 text-xs outline-none focus:border-primary"
        />
      )}
    </div>
  );
}

export const QuestionPrompt = React.memo(QuestionPromptInner);
