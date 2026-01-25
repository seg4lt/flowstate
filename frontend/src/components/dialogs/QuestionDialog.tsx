import { Button } from "../ui/button";
import { actions, useAppStore, type SendClientMessage } from "../../state/appStore";
import type { UserInputAnswer, UserInputQuestion } from "../../types";

interface Props {
  sendClientMessage: SendClientMessage;
}

export function QuestionDialog({ sendClientMessage }: Props) {
  const pendingQuestion = useAppStore((s) => s.pendingQuestion);
  const selections = useAppStore((s) => s.questionSelections);

  if (!pendingQuestion) return null;

  const questions = pendingQuestion.questions;
  const answered = questions.every((q) => hasAnswer(selections[q.id]));

  const submit = () => {
    if (!answered) return;
    const answers: UserInputAnswer[] = questions.map((q) => {
      const sel = selections[q.id] ?? { optionIds: [], freeformText: "" };
      const freeform = sel.freeformText.trim();
      if (freeform.length > 0) {
        return { questionId: q.id, optionIds: [], answer: freeform };
      }
      const pickedLabels = sel.optionIds
        .map((id) => q.options.find((o) => o.id === id)?.label)
        .filter((l): l is string => typeof l === "string");
      return {
        questionId: q.id,
        optionIds: sel.optionIds,
        answer: pickedLabels.join(", "),
      };
    });
    sendClientMessage({
      type: "answer_question",
      session_id: pendingQuestion.sessionId,
      request_id: pendingQuestion.requestId,
      answers,
    });
    actions.clearQuestion();
  };

  const cancel = () => {
    sendClientMessage({
      type: "cancel_question",
      session_id: pendingQuestion.sessionId,
      request_id: pendingQuestion.requestId,
    });
    actions.clearQuestion();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div className="fixed inset-0 flex items-center justify-center bg-background/70 backdrop-blur-sm z-50">
      <div className="w-[520px] max-w-[90vw] max-h-[85vh] overflow-y-auto rounded-lg border border-border bg-card text-card-foreground shadow-xl p-5 space-y-5">
        <h3 className="text-sm font-semibold">Question from agent</h3>
        {questions.map((q) => (
          <QuestionBlock
            key={q.id}
            question={q}
            selection={selections[q.id]}
            onKeyDown={onKeyDown}
          />
        ))}
        <div className="flex justify-end gap-2 pt-1">
          <Button size="sm" variant="outline" onClick={cancel}>
            Dismiss
          </Button>
          <Button size="sm" onClick={submit} disabled={!answered}>
            Send
          </Button>
        </div>
      </div>
    </div>
  );
}

interface BlockProps {
  question: UserInputQuestion;
  selection: { optionIds: string[]; freeformText: string } | undefined;
  onKeyDown: (e: React.KeyboardEvent) => void;
}

function QuestionBlock({ question, selection, onKeyDown }: BlockProps) {
  const sel = selection ?? { optionIds: [], freeformText: "" };
  const hasOptions = question.options.length > 0;

  return (
    <div className="space-y-2">
      {question.header && (
        <div className="inline-block text-[10px] font-semibold uppercase tracking-wide px-1.5 py-0.5 rounded bg-muted text-muted-foreground">
          {question.header}
        </div>
      )}
      <p className="text-sm leading-relaxed whitespace-pre-wrap">{question.text}</p>
      {hasOptions && (
        <div className="flex flex-col gap-1.5">
          {question.options.map((opt) => {
            const selected = sel.optionIds.includes(opt.id);
            return (
              <Button
                key={opt.id}
                size="sm"
                variant={selected ? "default" : "outline"}
                className="justify-start h-auto py-2 px-3 text-left"
                onClick={() =>
                  actions.setQuestionOption(question.id, opt.id, question.multiSelect)
                }
              >
                <div className="flex flex-col items-start gap-0.5">
                  <span className="text-sm font-medium">{opt.label}</span>
                  {opt.description && (
                    <span className="text-xs opacity-80 font-normal">
                      {opt.description}
                    </span>
                  )}
                </div>
              </Button>
            );
          })}
        </div>
      )}
      {question.allowFreeform && renderFreeform(question, sel, onKeyDown, hasOptions)}
    </div>
  );
}

function renderFreeform(
  q: UserInputQuestion,
  sel: { optionIds: string[]; freeformText: string },
  onKeyDown: (e: React.KeyboardEvent) => void,
  hasOptions: boolean,
) {
  const label = hasOptions ? "Other (type your own)" : "Your answer";
  const commonProps = {
    value: sel.freeformText,
    onChange: (e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) =>
      actions.setQuestionFreeform(q.id, e.target.value),
    onKeyDown,
    placeholder: label,
    className:
      "w-full bg-background border border-input rounded-md p-2 text-sm resize-none focus:outline-none focus:ring-1 focus:ring-ring",
  };
  if (q.isSecret) {
    return <input {...commonProps} type="password" autoComplete="off" />;
  }
  return <textarea {...commonProps} rows={3} />;
}

function hasAnswer(
  sel: { optionIds: string[]; freeformText: string } | undefined,
): boolean {
  if (!sel) return false;
  if (sel.freeformText.trim().length > 0) return true;
  if (sel.optionIds.length > 0) return true;
  return false;
}
