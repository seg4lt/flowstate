import { Button } from "../ui/button";
import { actions, useAppStore, type SendClientMessage } from "../../state/appStore";

interface Props {
  sendClientMessage: SendClientMessage;
}

export function QuestionDialog({ sendClientMessage }: Props) {
  const pendingQuestion = useAppStore((s) => s.pendingQuestion);
  const questionDraft = useAppStore((s) => s.questionDraft);

  if (!pendingQuestion) return null;

  const submit = () => {
    const answer = questionDraft.trim();
    if (!answer) return;
    sendClientMessage({
      type: "answer_question",
      session_id: pendingQuestion.sessionId,
      request_id: pendingQuestion.requestId,
      answer,
    });
    actions.clearQuestion();
  };

  return (
    <div className="fixed inset-0 flex items-center justify-center bg-background/70 backdrop-blur-sm z-50">
      <div className="w-[480px] max-w-[90vw] rounded-lg border border-border bg-card text-card-foreground shadow-xl p-5 space-y-4">
        <h3 className="text-sm font-semibold">Question from agent</h3>
        <p className="text-sm leading-relaxed whitespace-pre-wrap">
          {pendingQuestion.question}
        </p>
        <textarea
          value={questionDraft}
          onChange={(e) => actions.setQuestionDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              submit();
            }
          }}
          placeholder="Type your answer..."
          className="w-full min-h-[80px] bg-background border border-input rounded-md p-2 text-sm resize-none focus:outline-none focus:ring-1 focus:ring-ring"
          autoFocus
        />
        <div className="flex justify-end gap-2">
          <Button size="sm" variant="outline" onClick={() => actions.clearQuestion()}>
            Dismiss
          </Button>
          <Button size="sm" onClick={submit} disabled={!questionDraft.trim()}>
            Send
          </Button>
        </div>
      </div>
    </div>
  );
}
