/**
 * Live-preview extension bundle.
 *
 * Exposes a single `livePreview()` factory that the editor mounts as
 * an Extension array. Keeping the wiring in one place means the
 * editor component doesn't need to know about every internal bit.
 */

import "./style.css";
import { autocompletion } from "@codemirror/autocomplete";
import type { Extension } from "@codemirror/state";
import { linkAutocompleteSource } from "./link-autocomplete";
import { mermaidField } from "./mermaid-field";
import { livePreviewPlugin } from "./view-plugin";
import { linkClickHandler } from "./wikilink";

export interface LivePreviewOptions {
  /** Returns the directory of the currently-open `.md` file (absolute
   *  POSIX path). Used to resolve relative `![…](…)` paths into
   *  asset-protocol URLs. */
  getDocDir: () => string;
  /** Returns the absolute project root, or `""` if none is open. Used
   *  by the link-autocomplete source to expand project-relative paths
   *  out into absolute paths and back into doc-relative ones. */
  getProjectPath: () => string;
  /** Returns every project-relative path from the file index. */
  getFiles: () => string[];
  /** Returns the active app theme — drives the embedded
   *  `.excalidraw.svg` widget's re-export colour. */
  getTheme: () => "light" | "dark";
  /** Called on `Mod+click` of a standard `[label](url)` link. The host
   *  decides whether to open in the editor (relative `.md`), open
   *  externally (`https://…`), or do nothing. */
  onLinkOpen: (url: string) => void;
}

export function livePreview(opts: LivePreviewOptions): Extension {
  return [
    livePreviewPlugin(opts.getDocDir, opts.getTheme),
    mermaidField(),
    autocompletion({
      override: [
        linkAutocompleteSource(opts.getDocDir, opts.getProjectPath, opts.getFiles),
      ],
      activateOnTyping: true,
    }),
    linkClickHandler({ onLinkOpen: opts.onLinkOpen }),
  ];
}
