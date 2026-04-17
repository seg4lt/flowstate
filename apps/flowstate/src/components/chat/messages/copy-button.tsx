import * as React from "react";
import { Check, Copy } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useCopy } from "@/hooks/use-copy";
import { cn } from "@/lib/utils";

interface CopyButtonProps {
  /** The exact text to put on the clipboard. For agent replies pass
   *  the raw markdown source; for user messages the raw input text;
   *  for code blocks the raw code. Avoid passing rendered HTML. */
  text: string;
  /** Toast title shown on success. Defaults to "Copied". */
  label?: string;
  /** Native tooltip + aria-label. Defaults to "Copy". */
  title?: string;
  className?: string;
}

/**
 * Small ghost icon button that copies `text` to the clipboard and
 * flashes a Check icon for ~1.2s on success. Callers compose
 * visibility via className (e.g. `opacity-0 group-hover:opacity-100`)
 * so the button is unobtrusive until the user hovers the parent.
 */
export function CopyButton({
  text,
  label,
  title = "Copy",
  className,
}: CopyButtonProps) {
  const copy = useCopy();
  const [justCopied, setJustCopied] = React.useState(false);

  const onClick = React.useCallback(
    async (e: React.MouseEvent) => {
      // Stop bubbling so hovering a parent (e.g. the subagent
      // collapsible) doesn't toggle when the button is clicked.
      e.stopPropagation();
      await copy(text, label);
      setJustCopied(true);
      window.setTimeout(() => setJustCopied(false), 1200);
    },
    [copy, text, label],
  );

  return (
    <Button
      type="button"
      variant="ghost"
      size="icon-xs"
      onClick={onClick}
      title={title}
      aria-label={title}
      className={cn(
        "text-muted-foreground opacity-60 hover:opacity-100 focus-visible:opacity-100",
        className,
      )}
    >
      {justCopied ? (
        <Check className="h-3 w-3" />
      ) : (
        <Copy className="h-3 w-3" />
      )}
    </Button>
  );
}
