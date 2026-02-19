import * as React from "react";

interface UserMessageProps {
  input: string;
}

function UserMessageInner({ input }: UserMessageProps) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[80%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground">
        <p className="whitespace-pre-wrap">{input}</p>
      </div>
    </div>
  );
}

export const UserMessage = React.memo(
  UserMessageInner,
  (prev, next) => prev.input === next.input,
);
