// Tiny non-cryptographic content hash used to key the @pierre/diffs
// LRU cache. The LRU looks up by string cacheKey, so a stable hash
// of the file contents means: same bytes → same key → cache hit.
//
// Why not `JSON.stringify` or the raw string? The @pierre/diffs LRU
// holds the cacheKey in a Map; using the entire file body as the
// key would work but wastes memory (the LRU has 100 slots by
// default, a 1 MB file body in the key would mean 100 MB of keys
// just to look up cached results). A 32-bit djb2 hash is ~10 bytes
// base-36 and collision rates are fine for this scale (100-entry
// LRU keyed by ~a few hundred distinct file versions per session).
//
// Why not include a crypto-style hash (SHA-1 etc.)? WebCrypto is
// async and we need this inline in render-path cacheKey derivation.
// And we don't need cryptographic properties here — a collision
// would serve one file's tokenization for another, which is a
// quality bug, not a security one. 32-bit djb2 on file contents
// makes collisions vanishingly rare (~1 in 4 billion per pair).

export function hashContent(content: string): string {
  let h = 5381;
  // djb2: h = h * 33 ^ c. Iterate in JS without a typed array —
  // string charCodeAt is fast enough here (measured ~20 ns/char
  // in V8), so a 1 MB file hashes in ~20 ms — below the diff
  // IPC fetch cost, and runs only once per fetched file.
  for (let i = 0; i < content.length; i++) {
    h = ((h << 5) + h) ^ content.charCodeAt(i);
  }
  // Force unsigned 32-bit, then base-36 for compactness.
  return (h >>> 0).toString(36);
}
