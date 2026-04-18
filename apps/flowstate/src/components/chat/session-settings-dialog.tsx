import * as React from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useToast } from "@/hooks/use-toast";
import { useProviderFeatures } from "@/hooks/use-provider-features";
import { updateSessionSettings } from "@/lib/api";
import type { ProviderKind, SessionDetail } from "@/lib/types";

interface SessionSettingsDialogProps {
  open: boolean;
  onOpenChange: (next: boolean) => void;
  sessionId: string;
  /** The provider backing this session — drives which feature
   *  sub-sections are visible inside the dialog. */
  provider: ProviderKind;
  /** Current session detail, pulled from the React Query cache by
   *  the parent. We read `providerState.metadata` to seed the form;
   *  on save we fire the RPC and let the runtime broadcast its own
   *  TurnCompleted (or follow-up SessionLoaded) to refresh the
   *  cache — the dialog itself only needs the seed value. */
  session: SessionDetail | undefined;
}

/**
 * Per-session settings panel. Today this only carries the
 * "Compaction priorities" textarea — the steering text that gets
 * appended to the Claude SDK system prompt so the model honors it
 * when summarising older turns. Designed to grow: each new
 * per-session setting becomes another labelled row inside the
 * dialog body, gated on its own `ProviderFeatures` flag.
 *
 * The dialog reads the current value from
 * `session.providerState.metadata.compactCustomInstructions` and
 * writes via `update_session_settings`. Empty / whitespace-only
 * input clears the setting (the runtime stores the trimmed value
 * and the adapter's reader collapses empty to `None`). Settings
 * take effect on the NEXT turn the user fires.
 */
export function SessionSettingsDialog({
  open,
  onOpenChange,
  sessionId,
  provider,
  session,
}: SessionSettingsDialogProps) {
  const { toast } = useToast();
  const features = useProviderFeatures(provider);

  // Seed from the cache. We snapshot on `open` (not on every
  // render) so the user's in-progress edits don't get clobbered if
  // a turn-completion broadcast updates the underlying detail
  // mid-edit.
  const seedInstructions = React.useMemo(() => {
    const meta = session?.providerState?.metadata;
    if (meta && typeof meta === "object") {
      const v = (meta as { compactCustomInstructions?: unknown })
        .compactCustomInstructions;
      if (typeof v === "string") return v;
    }
    return "";
  }, [session?.providerState?.metadata]);

  const [instructions, setInstructions] = React.useState(seedInstructions);
  const [saving, setSaving] = React.useState(false);

  // Re-seed whenever the dialog re-opens. We don't reset on
  // `seedInstructions` changing alone (that would clobber edits if
  // the cache is updated while the user is typing).
  React.useEffect(() => {
    if (open) setInstructions(seedInstructions);
  }, [open, seedInstructions]);

  const dirty = instructions !== seedInstructions;
  const showCompactInstructions = !!features.compactCustomInstructions;

  async function handleSave() {
    setSaving(true);
    try {
      await updateSessionSettings(sessionId, {
        compactCustomInstructions: instructions,
      });
      toast({
        description: "Session settings saved.",
        duration: 2500,
      });
      onOpenChange(false);
    } catch (err) {
      toast({
        description: `Could not save: ${err instanceof Error ? err.message : String(err)}`,
        duration: 4500,
      });
    } finally {
      setSaving(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Session settings</DialogTitle>
          <DialogDescription>
            Per-session preferences. Changes take effect on the next turn
            you send.
          </DialogDescription>
        </DialogHeader>

        {showCompactInstructions ? (
          <div className="space-y-2 py-2">
            <label
              htmlFor="compact-custom-instructions"
              className="text-sm font-medium"
            >
              Compaction priorities
            </label>
            <p className="text-xs text-muted-foreground">
              Free-text guidance the model honors when summarising older
              turns to free up context. Leave empty to use the default.
            </p>
            <textarea
              id="compact-custom-instructions"
              value={instructions}
              onChange={(e) => setInstructions(e.target.value)}
              placeholder="e.g. Always preserve API contract decisions and the rationale behind them."
              rows={5}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 text-sm placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              spellCheck
              disabled={saving}
            />
          </div>
        ) : (
          <p className="py-4 text-sm text-muted-foreground">
            This provider doesn&apos;t expose any session-scoped settings yet.
          </p>
        )}

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={saving}
          >
            Cancel
          </Button>
          <Button onClick={handleSave} disabled={!dirty || saving}>
            {saving ? "Saving…" : "Save"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
