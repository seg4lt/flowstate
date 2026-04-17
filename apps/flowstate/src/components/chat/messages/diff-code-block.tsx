import * as React from "react";
import { PatchDiff } from "@pierre/diffs/react";
import { useTheme } from "@/hooks/use-theme";

interface DiffCodeBlockProps {
  code: string;
  /** Underlying language for syntax highlighting (used as file extension in
   *  synthetic diff headers so @pierre/diffs can infer the grammar). */
  language?: string;
}

/**
 * Ensures the diff text is a valid unified patch that PatchDiff can parse.
 * If it already has proper headers (--- / +++ / @@), return as-is.
 * Otherwise, wrap bare +/- lines with synthetic headers.
 */
function ensureUnifiedPatch(raw: string, ext: string): string {
  if (/^---\s/m.test(raw) && /^\+\+\+\s/m.test(raw) && /^@@\s/m.test(raw)) {
    return raw;
  }

  const lines = raw.split("\n");

  let oldCount = 0;
  let newCount = 0;
  for (const line of lines) {
    if (line.startsWith("-")) {
      oldCount++;
    } else if (line.startsWith("+")) {
      newCount++;
    } else {
      oldCount++;
      newCount++;
    }
  }

  return [
    `--- a/file.${ext}`,
    `+++ b/file.${ext}`,
    `@@ -1,${oldCount} +1,${newCount} @@`,
    ...lines,
  ].join("\n");
}

function DiffCodeBlockInner({ code, language = "tsx" }: DiffCodeBlockProps) {
  const { resolvedTheme } = useTheme();
  const patch = React.useMemo(
    () => ensureUnifiedPatch(code, language),
    [code, language],
  );

  return (
    <div className="mb-3 overflow-x-auto rounded-md border border-border text-xs last:mb-0">
      <PatchDiff
        patch={patch}
        options={{
          diffStyle: "unified",
          theme: { dark: "pierre-dark", light: "pierre-light" },
          // Track the app's theme preference rather than the OS so the
          // diff matches the rest of the markdown output.
          themeType: resolvedTheme,
          diffIndicators: "classic",
          overflow: "scroll",
          disableFileHeader: true,
          maxLineDiffLength: 2_000,
          tokenizeMaxLineLength: 5_000,
        }}
      />
    </div>
  );
}

export const DiffCodeBlock = React.memo(DiffCodeBlockInner);
