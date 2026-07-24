import assert from "node:assert/strict";
import { createServer } from "node:http";
import { performance } from "node:perf_hooks";

import { createMediaSlice, type MediaAttachment } from "./store/slices.ts";
import type { UploadResponse } from "./types.ts";

// Property 1: Bug Condition - Persisted Upload Settles as Matching Success
// **Validates: Requirements 1.1, 1.2, 1.3, 1.5, 1.6, 1.7, 1.8**
const EXPLORATION_SEED = 0xa77ac4e120250001n;

type MediaHarness = ReturnType<typeof createMediaSlice>;

function mediaHarness(): MediaHarness {
  let state = {} as MediaHarness;
  const set = (update: unknown) => {
    const patch = typeof update === "function"
      ? (update as (current: MediaHarness) => Partial<MediaHarness>)(state)
      : update as Partial<MediaHarness>;
    state = Object.assign(state, patch);
  };
  state = createMediaSlice(set as never, (() => state) as never, {} as never);
  return new Proxy({} as MediaHarness, {
    get: (_target, property) => state[property as keyof MediaHarness],
  });
}

async function settleThroughCurrentFrontendPath(
  state: MediaHarness,
  localId: string,
  response: Response,
): Promise<number> {
  if (!response.ok) {
    const error = await response.json().catch(() => ({ error: `HTTP ${response.status}` })) as { error?: string };
    throw new Error(error.error || `HTTP ${response.status}`);
  }
  const data: UploadResponse = await response.json() as UploadResponse;
  const errorMessage = data.error || null;
  if (errorMessage) throw new Error(errorMessage);

  const before = state.attachments.find((item) => item.id === localId);
  state.updateAttachment(localId, {
    id: data.id,
    filename: data.filename,
    uploading: false,
    extracted_text: "",
    image_count: data.image_count,
    images: undefined,
  });
  const after = state.attachments.find((item) => item.id === data.id);
  return before?.uploading === true && after?.uploading === false ? 1 : 0;
}

const fixtures = [
  { category: "markdown", serverId: "10000000-0000-4000-8000-000000000001", imageCount: 0 },
  { category: "png", serverId: "10000000-0000-4000-8000-000000000002", imageCount: 0 },
  { category: "pdf", serverId: "10000000-0000-4000-8000-000000000003", imageCount: 0 },
] as const;

const bodies = new Map(fixtures.map((fixture) => [
  `/${fixture.category}`,
  JSON.stringify({
    id: fixture.serverId,
    filename: `${fixture.category}.fixture`,
    extracted_text: "",
    image_count: fixture.imageCount,
    error: null,
  }),
]));

const server = createServer((request, serverResponse) => {
  const body = bodies.get(request.url || "");
  if (!body) {
    serverResponse.writeHead(404).end();
    return;
  }
  serverResponse.writeHead(200, {
    "content-type": "application/json",
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
    "content-length": Buffer.byteLength(body),
  });
  serverResponse.end(body);
});

await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
const address = server.address();
assert(address && typeof address !== "string");
console.error(`upload_exploration seed=0x${EXPLORATION_SEED.toString(16)}`);

try {
  for (const [index, fixture] of fixtures.entries()) {
    const state = mediaHarness();
    const localId = `20000000-0000-4000-8000-00000000000${index + 1}`;
    const chip: MediaAttachment = {
      id: localId,
      filename: "synthetic.fixture",
      extracted_text: "",
      image_count: 0,
      uploading: true,
    };
    state.addAttachment(chip);
    const started = performance.now();
    const fetchResponse: Response = await fetch(`http://127.0.0.1:${address.port}/${fixture.category}`);
    const bodyLength = Number(fetchResponse.headers.get("content-length") || 0);
    assert.equal(fetchResponse.status, 200, `category=${fixture.category} boundary=transport_status`);
    assert.match(
      fetchResponse.headers.get("content-type") || "",
      /^application\/json/,
      `category=${fixture.category} boundary=response_headers`,
    );
    const terminalTransitions = await settleThroughCurrentFrontendPath(state, localId, fetchResponse);
    const settled = state.attachments.find((item) => item.id === fixture.serverId);
    assert(settled, `category=${fixture.category} boundary=correlation event=valid_success`);
    assert.equal(settled.uploading, false, `category=${fixture.category} boundary=terminal_transition`);
    assert.equal(settled.error, undefined, `category=${fixture.category} boundary=terminal_transition indicator=red_x`);
    assert.equal(settled.id, fixture.serverId, `category=${fixture.category} boundary=server_id_binding`);
    assert.equal(terminalTransitions, 1, `category=${fixture.category} boundary=single_terminal`);
    console.error(
      `category=${fixture.category} phase=frontend_loopback_complete status=200 body_bytes=${bodyLength} elapsed_ms=${Math.round(performance.now() - started)}`,
    );
  }
} finally {
  await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}

console.log("attachment upload exploration checks passed");
