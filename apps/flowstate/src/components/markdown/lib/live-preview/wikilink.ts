/**
 * Wikilink + standard-link Mod+click handler.
 *
 * The view-plugin tags both styles:
 *   - `[[Note]]` → `.cm-md-wikilink` span carrying `data-wikilink`.
 *   - `[label](url)` → `.cm-md-link` span carrying `data-link-url`.
 *
 * **Locked decision** (per the plan): wikilinks render visually but do
 * NOT navigate on click. Standard `[label](url)` links DO follow on
 * Mod+click via the `onLinkOpen` callback.
 */

import { EditorView } from "@codemirror/view";

export function linkClickHandler(opts: { onLinkOpen: (url: string) => void }) {
  return EditorView.domEventHandlers({
    mousedown(event) {
      const isMod = navigator.platform.toLowerCase().includes("mac")
        ? event.metaKey
        : event.ctrlKey;
      if (!isMod) return;
      const target = event.target as HTMLElement | null;
      // Wikilinks are decorated for visual hint only — no navigation
      // by design. Skip them; fall through to standard-link branch.
      const link = target?.closest<HTMLElement>(".cm-md-link");
      if (!link) return;
      const url = link.dataset.linkUrl ?? "";
      if (!url) return;
      event.preventDefault();
      opts.onLinkOpen(url);
    },
  });
}
