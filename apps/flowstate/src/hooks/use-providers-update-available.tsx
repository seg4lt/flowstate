// Cross-page hook: returns `true` when at least one currently-enabled
// provider has reported (via its `health()` probe) that a newer
// version of its CLI is available. Drives:
//   - the small amber dot on the sidebar Settings icon
//     (`apps/flowstate/src/components/app-sidebar.tsx`)
//   - the per-row amber dot + Upgrade button inside Settings itself
//     (`apps/flowstate/src/components/settings/settings-view.tsx`)
//
// Disabled providers are deliberately excluded — the user has opted
// out of those, so they shouldn't drive a notification dot. This
// matches the new `dispatch_list_providers` filter on the daemon side
// and the per-row gate in `ProviderRow`.

import * as React from "react";
import { useApp } from "@/stores/app-store";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";

export function useProvidersUpdateAvailable(): boolean {
  const { state } = useApp();
  const { isProviderEnabled } = useProviderEnabled();
  return React.useMemo(
    () =>
      state.providers.some(
        (p) => p.updateAvailable && isProviderEnabled(p.kind),
      ),
    [state.providers, isProviderEnabled],
  );
}
