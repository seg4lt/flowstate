interface StreamingTextProps {
  content: string;
  reasoning?: string;
}

export function StreamingText({ content, reasoning }: StreamingTextProps) {
  if (!content && !reasoning) {
    return (
      <div className="flex justify-start">
        <div className="rounded-lg bg-muted px-3 py-2 text-sm text-muted-foreground">
          <span className="animate-pulse">Thinking...</span>
        </div>
      </div>
    );
  }

  if (!content && reasoning) {
    return (
      <div className="flex justify-start">
        <div className="max-w-[80%] rounded-lg bg-muted px-3 py-2 text-sm">
          <p className="whitespace-pre-wrap text-muted-foreground italic">
            {reasoning}
          </p>
          <span className="inline-block h-4 w-1 animate-pulse bg-muted-foreground" />
        </div>
      </div>
    );
  }

  return (
    <div className="flex justify-start">
      <div className="max-w-[80%] rounded-lg bg-muted px-3 py-2 text-sm">
        <p className="whitespace-pre-wrap">{content}</p>
        <span className="inline-block h-4 w-1 animate-pulse bg-foreground" />
      </div>
    </div>
  );
}
