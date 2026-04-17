import * as React from "react";
import { toast } from "@/hooks/use-toast";

/**
 * Single point of truth for copy-to-clipboard in the chat UI. Wraps
 * `navigator.clipboard.writeText` with a toast on success/failure so
 * callers never have to duplicate that plumbing. Returning a
 * stable callback (via useCallback) keeps memo'd components from
 * re-rendering when the hook is used as a prop.
 */
export function useCopy() {
  return React.useCallback(async (text: string, label = "Copied") => {
    try {
      await navigator.clipboard.writeText(text);
      toast({ title: label, duration: 1500 });
    } catch (err) {
      toast({
        title: "Copy failed",
        description: err instanceof Error ? err.message : String(err),
      });
    }
  }, []);
}
