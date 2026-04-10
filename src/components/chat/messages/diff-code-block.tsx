import * as React from "react";
import { PatchDiff } from "@pierre/diffs/react";

interface DiffCodeBlockProps {
  code: string;
}

/**
 * Ensures the diff text is a valid unified patch that PatchDiff can parse.
 * If it already has proper headers (--- / +++ / @@), return as-is.
 * Otherwise, wrap bare +/- lines with synthetic headers.
 */
function ensureUnifiedPatch(raw: string): string {
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
    "--- a/file",
    "+++ b/file",
    `@@ -1,${oldCount} +1,${newCount} @@`,
    ...lines,
  ].join("\n");
}

function DiffCodeBlockInner({ code }: DiffCodeBlockProps) {
  const patch = React.useMemo(() => ensureUnifiedPatch(code), [code]);

  return (
    <div className="mb-3 overflow-x-auto rounded-md border border-border text-xs last:mb-0">
      <PatchDiff
        patch={patch}
        options={{
          diffStyle: "unified",
          theme: { dark: "pierre-dark", light: "pierre-light" },
          themeType: "system",
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

export const DiffCodeBlock = React.memo(
  DiffCodeBlockInner,
  (prev, next) => prev.code === next.code,
);
