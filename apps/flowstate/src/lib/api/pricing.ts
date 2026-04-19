// Approximate per-model token pricing for Anthropic models, used to
// estimate cache savings on the Usage dashboard.
//
// IMPORTANT: these are display-only estimates, not authoritative
// billing figures. The dashboard's `Total spend` card uses
// `total_cost_usd` reported by the SDK directly — that number
// already accounts for the cache discount and is the one to trust
// for actual cost. The price table here exists *only* so we can
// answer "what did caching save you?" by comparing what the
// cache_read tokens would have cost at the regular input rate vs
// what they actually cost at the cache-read rate.
//
// Numbers reflect Anthropic's public list pricing as of the
// 2025–2026 model lineup. If pricing drifts, savings estimates
// drift with it; the headline cost figure stays correct because
// it's SDK-reported.
//
// Units: USD per 1,000,000 tokens.
export interface ModelPricing {
  inputPerMTok: number;
  cacheReadPerMTok: number;
  cacheWritePerMTok: number;
  outputPerMTok: number;
}

// Family-level rates. We normalize specific model ids (with or
// without date suffixes) onto these families.
const SONNET: ModelPricing = {
  inputPerMTok: 3.0,
  cacheReadPerMTok: 0.3,
  cacheWritePerMTok: 3.75,
  outputPerMTok: 15.0,
};

const OPUS: ModelPricing = {
  inputPerMTok: 15.0,
  cacheReadPerMTok: 1.5,
  cacheWritePerMTok: 18.75,
  outputPerMTok: 75.0,
};

const HAIKU: ModelPricing = {
  inputPerMTok: 1.0,
  cacheReadPerMTok: 0.1,
  cacheWritePerMTok: 1.25,
  outputPerMTok: 5.0,
};

// Match on substrings rather than exact ids so pinned date
// variants (e.g. `claude-sonnet-4-5-20250929`) and dotted aliases
// (`claude-sonnet-4.5`) both resolve. Order matters: check more
// specific names first if any are added.
const FAMILY_MATCHERS: Array<{ test: RegExp; pricing: ModelPricing }> = [
  { test: /opus/i, pricing: OPUS },
  { test: /haiku/i, pricing: HAIKU },
  { test: /sonnet/i, pricing: SONNET },
];

/// Resolve pricing for a model id. Returns null when the id
/// doesn't match any known family — the caller should treat that
/// as "savings unknown for this slice" rather than substituting a
/// default (we'd rather show "$X+" than misattribute).
export function pricingForModel(model: string | null | undefined): ModelPricing | null {
  if (!model) return null;
  for (const m of FAMILY_MATCHERS) {
    if (m.test.test(model)) return m.pricing;
  }
  return null;
}

/// Estimate the dollar amount that caching saved on a given slice
/// of usage. `cacheReadTokens` is the number of tokens served from
/// the prompt cache; the savings are the delta between what those
/// tokens *would* have cost at the regular input rate and what
/// they *did* cost at the cache-read rate.
///
/// Returns null when the model is unknown — caller should display
/// the count without a dollar figure rather than guess.
export function estimateCacheReadSavingsUsd(
  model: string | null | undefined,
  cacheReadTokens: number,
): number | null {
  const price = pricingForModel(model);
  if (!price) return null;
  const wouldHaveCost = (cacheReadTokens / 1_000_000) * price.inputPerMTok;
  const actuallyCost = (cacheReadTokens / 1_000_000) * price.cacheReadPerMTok;
  return wouldHaveCost - actuallyCost;
}
