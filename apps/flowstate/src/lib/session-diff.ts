import type { GitFileSummary } from "./api";

// Re-export under the existing name so HeaderActions and DiffPanel
// keep their imports stable. Stats now come straight from
// `git diff --numstat` on the rust side, so there's nothing for
// the frontend to compute — we just rename the type.
export type AggregatedFileDiff = GitFileSummary;
