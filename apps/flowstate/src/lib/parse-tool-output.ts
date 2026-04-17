/**
 * Parses sub-agent tool output which may be a JSON-encoded content-block
 * array (e.g. `[{"type":"text","text":"## Markdown …"}]`) and extracts
 * the concatenated text.  Falls back gracefully for plain-text output.
 */
export function extractToolOutputText(output: string): {
  text: string;
  isMarkdown: boolean;
} {
  let parsed: unknown;
  try {
    parsed = JSON.parse(output);
  } catch {
    return { text: output, isMarkdown: false };
  }

  if (!Array.isArray(parsed)) {
    return { text: output, isMarkdown: false };
  }

  const texts = parsed
    .filter(
      (item): item is { type: string; text: string } =>
        item != null &&
        typeof item === "object" &&
        item.type === "text" &&
        typeof item.text === "string",
    )
    .map((item) => item.text);

  if (texts.length === 0) {
    return { text: output, isMarkdown: false };
  }

  return { text: texts.join("\n\n"), isMarkdown: true };
}
