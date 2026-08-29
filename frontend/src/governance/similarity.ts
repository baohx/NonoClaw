// Similarity primitives for trajectory governance — lexical (n-gram cosine),
// dependency-free. Mirrors the "consume-only, no kernel modification"
// philosophy of the DSH dsh-trajectory-governance plugin: everything here is a
// pure function over strings / JSON scalars, so loop-deadlock and goal-drift
// detection never need an embedding service.

const DEFAULT_NGRAM = 3;
const EMBED_DIM = 256;

/** Character n-grams of `text` (lowercased). Short inputs degrade n so a
 * 2-char or 1-char string still yields at least one token. */
export function ngramTokens(text: string, n = DEFAULT_NGRAM): string[] {
  const s = text.toLowerCase();
  if (s.length === 0) return [];
  const effective = Math.min(n, s.length);
  const tokens: string[] = [];
  for (let i = 0; i <= s.length - effective; i += 1) {
    tokens.push(`g:${s.slice(i, i + effective)}`);
  }
  return tokens;
}

/** Word unigrams (lowercased, CJK-aware word boundaries). */
export function wordTokens(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9\u4e00-\u9fff]+/)
    .filter(Boolean)
    .map((word) => `w:${word}`);
}

/** Mixed token stream: word unigrams + character n-grams. Word overlap gives
 * semantic similarity; n-grams give robustness to spelling/order variation. */
function mixedTokens(text: string): string[] {
  return [...wordTokens(text), ...ngramTokens(text)];
}

function countMap(tokens: string[]): Map<string, number> {
  const map = new Map<string, number>();
  for (const token of tokens) {
    map.set(token, (map.get(token) ?? 0) + 1);
  }
  return map;
}

function magnitude(map: Map<string, number>): number {
  let sum = 0;
  for (const count of map.values()) sum += count * count;
  return Math.sqrt(sum);
}

/** Multiset cosine similarity between two token lists, in [0, 1]. */
export function cosineOf(a: string[], b: string[]): number {
  const left = countMap(a);
  const right = countMap(b);
  let dot = 0;
  for (const [token, count] of left) {
    const other = right.get(token);
    if (other !== undefined) dot += count * other;
  }
  const denom = magnitude(left) * magnitude(right);
  return denom === 0 ? 0 : dot / denom;
}

/** Text similarity via mixed word + character n-gram cosine, in [0, 1]. */
export function textSimilarity(a: string, b: string): number {
  if (a === b) return 1;
  if (a.length === 0 || b.length === 0) return 0;
  return cosineOf(mixedTokens(a), mixedTokens(b));
}

/** Canonical, order-insensitive string form of a JSON value. */
function canonical(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value !== "object") return String(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const obj = value as Record<string, unknown>;
  const keys = Object.keys(obj).sort();
  return `{${keys.map((key) => `${key}:${canonical(obj[key])}`).join(",")}}`;
}

/** Argument similarity between two tool inputs, in [0, 1]. Both empty = 1. */
export function argumentsSimilarity(a: unknown, b: unknown): number {
  const ca = canonical(a);
  const cb = canonical(b);
  if (ca === "" && cb === "") return 1;
  if (ca === "" || cb === "") return 0;
  return textSimilarity(ca, cb);
}

export interface ToolCallLike {
  name: string;
  input?: unknown;
}

/** Tool-call similarity: 0.6 name identity + 0.4 argument overlap, in [0, 1]. */
export function toolCallSimilarity(a: ToolCallLike, b: ToolCallLike): number {
  const nameSim = a.name === b.name ? 1 : 0;
  const argSim = argumentsSimilarity(a.input, b.input);
  return 0.6 * nameSim + 0.4 * argSim;
}

/** Error-text similarity (n-gram cosine over message content), in [0, 1]. */
export function errorSimilarity(a: string, b: string): number {
  return textSimilarity(a, b);
}

// ── Lexical embedder (feature-hashed n-gram bag → normalized vector) ──────

/** FNV-1a 32-bit hash — deterministic, no dependencies. */
function fnv1a(str: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < str.length; i += 1) {
    hash ^= str.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

/** Map text to a normalized EMBED_DIM feature vector via FNV-hashed mixed
 * tokens (word unigrams + char n-grams). */
export function embed(text: string): Float64Array {
  const vector = new Float64Array(EMBED_DIM);
  for (const token of mixedTokens(text)) {
    vector[fnv1a(token) % EMBED_DIM] += 1;
  }
  let norm = 0;
  for (let i = 0; i < EMBED_DIM; i += 1) norm += vector[i] * vector[i];
  norm = Math.sqrt(norm);
  if (norm > 0) {
    for (let i = 0; i < EMBED_DIM; i += 1) vector[i] /= norm;
  }
  return vector;
}

/** Cosine between two embedding vectors, in [0, 1] (non-negative features). */
export function vectorCosine(a: Float64Array, b: Float64Array): number {
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < EMBED_DIM; i += 1) {
    dot += a[i] * b[i];
    na += a[i] * a[i];
    nb += b[i] * b[i];
  }
  const denom = Math.sqrt(na) * Math.sqrt(nb);
  return denom === 0 ? 0 : dot / denom;
}
