# Attachment Upload False Failure Bugfix Design

## Overview

本设计修复 Web UI 附件上传中“服务端已成功提取并保存，前端最终仍显示失败”的跨边界假失败。修复范围仅覆盖现有 `/api/upload`、附件 chip 状态以及现有 `AttachmentRef` 提交流程；不增加文件格式、不改变 OCR/文档提取策略、不放宽鉴权、大小、签名、UUID、路径或私有存储边界。

当前代码可以证明成功路径的静态结构，但不能仅凭已有日志证明现场请求的 HTTP 响应已被浏览器完整接收，也不能证明部署时 Rust response 与 TypeScript 解析契约一致。因此实现顺序必须是：先用未修复代码建立端到端时间线并取得反例，再定位失败边界，最后实施最小修复和契约/状态机加固。若诊断反驳本设计中的根因假设，应回到本设计更新假设，不得修改文档提取算法碰运气。

目标链路为：

```text
File input / drop / paste
  -> client requestId + stable localId
  -> POST /api/upload (optional auth token, multipart field "file")
  -> auth/config/size/type/signature/storage checks
  -> existing Markdown/PDF/OCR processing
  -> private source file + attachment.json persisted
  -> HTTP 200 application/json UploadSuccessResponse
  -> one JSON parse + runtime schema validation
  -> correlate by requestId, attach serverId to stable localId
  -> exactly one succeeded/failed terminal state
  -> succeeded serverId becomes AttachmentRef.id on message submit
  -> server loads attachment.json and performs existing prompt/image injection
```

## Glossary

- **Bug_Condition (C)**: 服务端处理和结果保存成功，但有效的前端上传请求在确定时限内未进入匹配成功终态，或错误进入失败终态的条件。
- **Property (P)**: 对满足 `C(X)` 的上传，浏览器接受有效成功响应后，匹配请求必须且只能进入一次 `succeeded`，并保留正确服务端 attachment ID。
- **Preservation**: 对不满足 `C(X)` 的输入，修复前后必须保持上传接受/拒绝、安全决定、提取结果、附件注入和 session 副作用一致。
- **requestId**: 每次 `uploadFile` 调用生成的规范 UUID；在请求整个生命周期内不变，是 reducer、AbortController、timer 和诊断的唯一操作身份。
- **localId**: 前端 attachment chip 的稳定临时 UUID；只用于 UI identity 和移除，不发送为服务端附件引用，也不在成功时改写。
- **serverId**: `upload_handler` 生成并写入服务端私有存储目录的规范 UUID；仅在成功 payload 校验通过后绑定到对应 `requestId/localId`，消息提交时作为 `AttachmentRef.id`。
- **UploadSuccessResponse**: `/api/upload` 的 200 JSON 成功 payload。Rust 权威定义位于 `rust/crates/cli/src/serve_http/protocol.rs`，TypeScript 对应定义和运行时解析位于 `frontend/src/types.ts` 与拟新增的 `frontend/src/attachment-upload.ts`。
- **UploadErrorResponse**: 非 2xx JSON 错误 envelope，沿用 `AppError` 的 `error/code/retryable/operation/trace_id/safe_details` 字段。
- **Accepted terminal event**: reducer 在请求仍有效、generation 匹配且当前状态非终态时接受的首个 `success`、`failure`、`timeout` 或 `abort` 事件。
- **Late event**: 首个终态之后到达的 response、parse completion、timeout、abort、重复 completion 或旧闭包更新；必须是无状态副作用的 no-op。
- **Settlement deadline**: 从请求进入 `uploading` 起计算的 120,000 ms 固定策略值；实现为可注入 clock/timer 的模块常量，测试不依赖真实等待。该值不成为用户可配置功能。
- **Generation**: 捕获上传开始时的 session identity；session 切换或组件销毁会使旧请求失效，防止旧结果写入新 session。

## Bug Details

### Current End-to-End Upload Sequence

代码证据给出的当前时序如下：

1. `frontend/src/components/InputBox.tsx::handleFiles` 对扩展名做客户端 allowlist 过滤，然后逐文件调用 `uploadFile`；多附件通过多个独立异步调用并发执行。
2. `uploadFile` 生成一个临时 UUID，立即通过 `addAttachment` 加入 `{ id, filename, uploading: true }` chip。
3. 浏览器向 `authenticatedApiUrl("/api/upload")` 发起 `fetch`，body 为仅含 `file` 字段的 `FormData`。当前没有应用级 timeout、`AbortController`、request identity header 或 in-flight registry。
4. `rust/crates/cli/src/serve_http/upload_service.rs::upload_handler` 依次执行鉴权、doc model 配置校验、multipart 单文件读取、32 MiB 限制、扩展名/签名校验、文件名净化、规范 UUID 生成和私有源文件写入。
5. `rust/crates/cli/src/attachments.rs::process_file` 保持现有分流：Markdown/TXT 直接读取；PDF 先 `pdftotext`/`pdfimages`，必要时 OCR；PNG/JPEG 进入配置的 OCR provider。
6. 提取成功后，handler 将文本和图片写入私有 `attachment.json`。只有该写入成功，才调用 `json_response(StatusCode::OK, UploadResponse)`。
7. 当前成功响应为内存中一次 `serde_json::to_vec` 后构造的 HTTP 200 body，headers 至少为 `content-type: application/json`、`cache-control: no-store` 和 `x-content-type-options: nosniff`。成功 body 当前包含 `id`、`filename`、空 `extracted_text`、`image_count`、可省略的 `images` 和 `error: null`。
8. 前端先检查 `resp.ok`，再以 TypeScript annotation（非运行时校验）执行一次 `resp.json()`，将 truthy `data.error` 视为失败。
9. 成功时前端以旧临时 `id` 查找 chip，同时把 chip 的 `id` 改写为服务端 `data.id`；失败 catch 仍以旧临时 `id` 更新。当前 `updateAttachment` 不报告未匹配更新。
10. 提交消息时，前端筛选 `!error && !uploading` 的附件，将当前 `id` 作为 `AttachmentRef.id`。`run_handler.rs::enrich_prompt_with_attachments` 用该 ID 读取 `attachment.json`，并保持现有服务端文本/图片注入与 legacy inline fallback。

以上只能证明 handler 的代码会构造响应，不能证明现场 socket/proxy 已完成传输或浏览器已收到同一版本 payload。当前应用代码没有 timeout，因此现场“等待后红 X”在当前源码中只能由 fetch 拒绝、非 2xx、JSON 解析异常或非空 `error` 进入 catch；单纯 `updateAttachment` 未命中只会留下 uploading，不会自行变成红 X。这一事实用于缩小诊断范围，但不是根因结论。

### Bug Condition

**Formal Specification:**

```pascal
FUNCTION isBugCondition(input)
  INPUT: input of type AttachmentUploadTrace
  OUTPUT: boolean

  RETURN input.backend_success = true
     AND input.backend_attachment_result_exists = true
     AND input.explicit_abort_before_success = false
     AND input.external_transport_failure = false
     AND (
       input.frontend_terminal_state = failed
       OR (
         input.frontend_state IN {pending, uploading}
         AND input.settlement_deadline_elapsed = true
       )
     )
END FUNCTION
```

### Concrete Examples

- **PNG OCR**: DeepSeek OCR 完成并记录约 `pages=1 chars=503`，`attachment.json` 已存在；预期匹配 chip 显示成功，实际长时间处理中后出现红色 X。
- **Markdown**: `.md` 已进入 `text/markdown` 直接读取并保存；预期成功并可随消息提交，实际最终显示红色 X。
- **PDF**: 文本与图片提取完成并记录约 `chars=15967 images=0`，结果已保存；预期成功并沿既有注入路径使用，实际最终显示红色 X。
- **并发边界**: A、B 两附件同时上传，B 先完成；预期 B 只更新 B，随后 A 只更新 A。数组 index、文件名或完成顺序均不得参与关联。
- **timeout 边界**: 有效成功 payload 在 deadline event 被 reducer 接受前完成校验；预期 success 获胜，晚到 timeout/abort 不得降级。若 timeout 先被接受并调用 abort，随后到达的成功必须忽略且不得更新其他 chip。
- **真实失败边界**: 签名与扩展名不匹配返回非 2xx structured error；该输入不属于 `C(X)`，必须继续失败，不能因本修复被标成成功。

### Root-Cause Evidence Required

在修改生产逻辑前，必须对未修复版本为每个 fixture 收集以下同一请求证据。所有日志只记录 UUID、阶段、status、字节数、耗时和类别，不记录文件名、路径、提取文本、图片/base64、token、请求/响应原文或上游私密错误。

| 边界 | 必需证据 | 可确认/排除的问题 |
|---|---|---|
| Browser request start | `requestId`、session generation、开始时间、文件类型类别和大小（不含名称） | 请求是否真正发出、是否被旧 session 清理 |
| Browser Network response | status、`content-type`、response headers、transfer timing、body byte length、是否被 browser/proxy abort | HTTP 是否完整到达；区分非 2xx、连接中断和等待 |
| Handler milestones | `request_id`、`upload_id`、`operation=upload`、`phase=received/stored/extracted/metadata_stored/response_built`、elapsed_ms | 后端成功事实、响应是否被构造；不能单独证明客户端收到 |
| Success serialization | Rust value→bytes round-trip、status 200、body schema、body length | Rust 序列化与成功 envelope 是否正确 |
| Browser decode | `json_read_ok`、`schema_valid`、拒绝类别；禁止记录 payload | JSON 与 schema 边界、`error:null/absent` 解释 |
| Correlation | `requestId -> localId -> serverId` 的 UUID-only 记录和 reducer disposition | 未知 ID、串写、重复/晚到更新 |
| Terminal transition | accepted event category、state before/after、elapsed_ms | timeout/abort/response 的胜者和单终态 |
| Deployment identity | 前端 asset build/version 与服务端 binary version（不含环境秘密） | 旧前端/新服务端 schema skew 或缓存资产 |

浏览器 DevTools 抓包仅用于本地诊断，不提交含附件内容或 auth query token 的 HAR。自动测试使用 synthetic payload 和无敏感 fixture。

## Expected Behavior

### Expected-Behavior Predicate

```pascal
FUNCTION expectedBehavior(result)
  INPUT: result of type AttachmentUploadOutcome
  OUTPUT: boolean

  RETURN result.http_status IN 200..299
     AND result.content_type = "application/json"
     AND isValidUploadSuccessPayload(result.response_body)
     AND result.frontend_terminal_state = succeeded
     AND result.frontend_server_id = result.persisted_attachment_id
     AND result.terminal_transition_count = 1
     AND result.failure_indicator_visible = false
END FUNCTION
```

### Preservation Requirements

**Unchanged Behaviors:**

- 文件选择、拖放、粘贴和多附件上传入口继续使用当前扩展名 allowlist；不增加或删除格式。
- Markdown/TXT 直接读取、PDF 文本/图片提取与 fallback OCR、PNG/JPEG OCR、DOC/DOCX 转换和嵌入图片处理保持原语义。
- `/api/upload` 路径、multipart `file` 字段、可选 query token、HTTP 200 成功语义以及当前兼容 response 字段保持兼容。
- 成功附件仍只向浏览器返回引用元数据，原始提取文本和图片保留在服务端私有存储；消息提交仍通过 `AttachmentRef.id` 加载并注入。
- 非授权、配置错误、超限、不支持/签名不匹配、存储/提取失败和独立网络失败继续返回真实失败。
- 32 MiB 上限、文件名净化、规范 UUID、`0700` 目录、`0600` 文件、canonical path/traversal 防护不变。
- session 切换、清理、断线重连和页面销毁继续清理当前 UI 状态；旧请求不得恢复到新 session。
- 现有 WebSocket `ClientMsg`、`ServerMsg`、`EngineEvent` 标签和单一权威映射不因 HTTP 上传修复改变。

**Scope:**

所有不满足 `isBugCondition` 的输入必须保持修复前的可观察行为，包括：

- 所有真实上传失败和显式取消；
- 非附件输入、voice/STT、消息发送和 WebSocket 生命周期；
- 已成功且没有发生假失败的现有客户端；
- legacy inline attachment fallback；
- 文件处理输出与 provider 调用语义；
- 安全拒绝、日志脱敏和服务端私有数据边界。

允许的差异仅为新增 UUID-only 诊断字段、明确的前端错误类别、稳定 local identity，以及测试中对 UUID、trace ID、时间戳、elapsed time 和 JSON 字段顺序的规范化。

## Hypothesized Root Cause

### Evidence-Backed Findings

1. **服务端成功结果与 HTTP 客户端成功不是同一事实**：现有后端处理日志能证明 extraction 完成，但没有覆盖 browser receive、JSON parse 和 reducer settlement。
2. **当前前端缺少运行时 contract validation**：`const data: UploadResponse = await resp.json()` 只影响编译期，不验证部署 payload；畸形/错误类型字段可能流入状态更新。
3. **Rust/TypeScript 对 `error` 的静态表达不完全一致**：Rust `Option<String>` 在当前成功响应序列化为 `null`，TypeScript 声明为 `error?: string`。当前 `data.error || null` 会把 `null` 正确视为无错误，所以该差异本身不能解释已观察缺陷，但应由 wire fixture 消除歧义。
4. **当前身份字段承担两个角色**：chip 临时 `id` 在成功时被改成 server ID，后续旧闭包仍持有临时 ID。首个成功更新通常可以命中，因此这不是已证根因；但缺少 request identity、未命中检测和单终态约束，无法可靠处理重复、乱序和 session race。
5. **当前源码没有应用级 timeout/abort**：若现场在等待后进入 catch，需检查浏览器、service worker、反向代理、移动网络或部署版本是否产生了外部 timeout/abort；不能把不存在于当前源码的 timer 当作根因。
6. **当前成功 Rust value 仅包含可直接序列化字段**：正常 `serde_json::to_vec` 失败概率低；但 handler 级契约、fallback error envelope 和实际部署版本仍需测试，不能仅凭静态阅读排除。

### Ranked Hypotheses and Diagnostic Gates

1. **响应传输或部署边界在 backend extraction 后失败**（与“等待后红 X”最吻合，但未确认）
   - 检查 handler `response_built` 是否存在、浏览器是否获得 status/headers/body、是否出现 connection reset、proxy timeout、service worker 或 stale asset。
   - 只有在同一 `requestId` 显示 response 未完整到达时，才修改对应 HTTP/部署边界；不得修改 OCR。
2. **浏览器收到的不是当前有效成功 schema**
   - 保存无敏感 synthetic wire bytes 的契约测试；在本地 Network 面板只记录 status/content type/body schema 结果。
   - 若是 HTML fallback、空 body、畸形 JSON、字段类型错误或旧 schema，修复 response routing/version skew 或 parser contract。
3. **请求完成后关联到无效/旧 chip 或 session generation**
   - 用 reducer disposition 证明 success 是否因未知 `requestId` 被拒绝、是否错误使用旧临时 ID、数组 index 或已清理 session。
   - 只有取得关联反例后，才把它列为现场根因；无论是否为现场根因，稳定三 ID 模型用于满足并发和竞态需求。
4. **timeout/abort 与成功的竞争**
   - 当前源码无该机制；先确认现场 bundle 或外层网络是否注入 abort。修复后新增的应用 timeout 必须由纯 reducer 顺序定义，不能引入新的假失败 race。
5. **成功 response 被错误 envelope 或 truthy `error` 污染**
   - 当前 handler 显式构造 `error: None`；用 Rust round-trip 和 TS parser fixture 验证。若实际 body 有非空 error，追查部署 binary 或中间层，不在前端静默忽略。

在证据不足时，根因状态为 **未确认**。实现阶段的 exploratory test 必须先在未修复代码上产生至少一个可定位反例；若所有 fixture 均通过，应采集现场部署证据并更新 requirements/design，而不是宣称修复完成。

## Correctness Properties

Property 1: Bug Condition - Persisted Upload Settles as Matching Success

_For any_ attachment upload trace where `isBugCondition` holds, the fixed upload flow SHALL expose a valid 2xx JSON success response, bind the persisted canonical server attachment ID to the same request/local attachment, accept exactly one `succeeded` terminal transition before the deterministic deadline, and never display the red failure indicator.

**Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5**

Property 2: Preservation - Non-Bug Upload Behavior and Security

_For any_ input where `isBugCondition` does not hold, the fixed flow SHALL produce the same normalized acceptance/rejection decision, extraction output, image count, attachment reference, real error classification, security decision, session effect and message-injection behavior as the original flow; normalization may ignore generated UUIDs, trace IDs, timestamps, elapsed time and JSON object ordering only.

**Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8**

Property 3: Success Wire Contract - Rust/TypeScript Semantic Round Trip

_For any_ valid Rust upload success record, serialization followed by the TypeScript runtime parser SHALL preserve canonical `id`, `filename`, non-negative integer `image_count` and compatible fields, SHALL interpret `error` as no error only when it is `null` or absent, and SHALL reject malformed JSON, missing/incorrect fields, non-canonical IDs and non-null success errors.

**Validates: Requirements 2.4, 2.5, 2.8**

Property 4: Correlation - Deterministic Single-Terminal Isolation

_For any_ generated interleaving of start, valid success, failure, timeout, abort, duplicate completion, session invalidation and late events across distinct requests, each request SHALL accept at most one terminal transition, SHALL mutate only its owned local attachment, SHALL preserve an accepted success against later events, and SHALL ignore success arriving after an accepted timeout/abort without changing another attachment.

**Validates: Requirements 2.6, 2.8, 3.6**

Property 5: Failure Contract - Structured and Redacted Diagnostics

_For any_ real server, transport, parse, correlation, timeout, abort or state-conflict failure and any generated sensitive internal detail, the externally visible failure SHALL contain a stable `code`, `retryable`, `operation`, `trace_id` and bounded safe correlation data, SHALL select the corresponding safe user message, and SHALL not contain paths, attachment content, image data, credentials, authorization values or private upstream errors.

**Validates: Requirements 2.7, 2.8, 3.3, 3.4, 3.5**

## Fix Implementation

### Contract and Ownership Decisions

#### HTTP success contract

`POST /api/upload` remains the endpoint and continues to return HTTP 200 on success.

```typescript
interface UploadSuccessResponse {
  id: string;                 // canonical lowercase hyphenated UUID
  filename: string;           // server-sanitized filename
  extracted_text: string;     // current server emits ""; accepted only for compatibility, never stored/logged
  image_count: number;        // finite non-negative integer
  images?: ImageRef[];        // legacy-compatible; current server omits; never copied into browser store
  error?: null;               // null or absent means success; string/non-null is invalid in a 2xx payload
}
```

Success response requirements:

- status is exactly 200 for the current API;
- `content-type` starts with `application/json`;
- `cache-control: no-store` and `x-content-type-options: nosniff` remain;
- body is one complete JSON object and `id` equals the persisted `StoredAttachment.id`;
- current server continues to emit empty `extracted_text`, omit `images`, and emit `error: null` for wire compatibility;
- response must not contain source bytes, extracted body, base64 images, path or credentials;
- serialization failure returns HTTP 500 `UploadErrorResponse`, never a 2xx body.

#### HTTP error contract

All non-2xx paths use:

```typescript
interface UploadErrorResponse {
  error: string;              // safe user-facing message
  code: string;               // existing ErrorCode wire value
  retryable: boolean;
  operation: string;          // "upload" or "serialize_response"
  trace_id: string;           // canonical safe correlation UUID
  safe_details: Record<string, unknown>;
}
```

`http_error.rs::json_response` serialization fallback currently lacks `trace_id` and `x-content-type-options`; it must be aligned with the structured contract without leaking the original serialization error. Existing status/code mappings for auth, configuration, payload size, unsupported format, invalid request, storage and extraction remain unchanged.

#### Request identity and response correlation

- Frontend creates distinct `requestId` and `localId` UUIDs before adding a chip.
- Frontend sends `x-upload-request-id: <requestId>`; token behavior and multipart body remain unchanged.
- Handler accepts the header additively. Missing/invalid values do not weaken auth and do not reject legacy clients; the handler generates a canonical correlation UUID instead. Only validated UUIDs enter logs or response headers.
- Handler reflects the effective UUID as `x-upload-request-id` and emits an `x-trace-id`; errors use the same trace in body/header. These headers are additive and contain no secret.
- The server generates `serverId` independently. Frontend correlation is `requestId -> localId -> serverId`; filename, array index, completion order and server ID are never lookup keys before success validation.
- `localId` remains the React key and remove key. `serverId` is optional until success and is the only value mapped to `AttachmentRef.id` on submit.

### Frontend State Machine

The pure state transition owner belongs in `frontend/src/attachment-upload.ts` (or the existing pure transition module if implementation keeps a single owner). `InputBox.tsx` orchestrates I/O only; it must not implement ad hoc terminal writes.

```text
pending --start--------------------------> uploading
pending --preflight failure--------------> failed (terminal)
uploading --valid correlated success----> succeeded (terminal)
uploading --server/transport/parse error-> failed (terminal)
uploading --timeout accepted------------> failed:timeout (terminal), then controller.abort()
uploading --abort accepted--------------> failed:cancelled (terminal)
terminal --any duplicate/late event------> same terminal state (no-op)
invalid generation --any completion------> ignored (no attachment mutation)
```

Proposed internal state shape:

```typescript
type UploadPhase = "pending" | "uploading" | "succeeded" | "failed";

interface MediaAttachment {
  localId: string;
  requestId: string;
  serverId?: string;
  filename: string;
  extracted_text: "";
  image_count: number;
  images?: undefined;
  phase: UploadPhase;
  failure?: UploadFailure;
  sessionGeneration: string;
}
```

Transition rules:

1. A transition resolves ownership by `requestId`, verifies expected `localId` and generation, then checks `phase`.
2. Only `pending`/`uploading` can accept a terminal event. The reducer returns `{state, disposition}` so unknown, stale and duplicate events are observable in safe diagnostics rather than silent mutation failures.
3. A valid success atomically sets `serverId`, canonical filename, image count and `phase=succeeded`; it clears failure, timer and in-flight ownership.
4. A failure atomically sets `phase=failed` and a safe structured `UploadFailure`; it never sets `serverId`.
5. The first event invocation accepted by the single-threaded reducer wins. A validated success accepted before timeout/abort cannot be downgraded. If timeout/abort is accepted first, its controller is aborted and all later response work is ignored.
6. Timeout begins only after `start`; fixed deadline is 120,000 ms. Tests inject fake `now`, `setTimer` and `clearTimer`.
7. `InputBox` owns a `Map<requestId, {localId, controller, timer, generation}>`. Success/failure removes the entry and clears its timer exactly once.
8. Session change/component cleanup aborts all in-flight controllers and invalidates generation before clearing chips. Any resulting catch/late response is ignored. This preserves current boundary cleanup and prevents resurrection.
9. Concurrent files get independent controller/timer entries. `Promise` completion order has no effect on ownership.
10. Submit remains disabled while any attachment is `pending/uploading`; it excludes failed items and maps each succeeded `serverId` to `AttachmentRef.id`.
11. Removal remains available only for terminal chips, preserving current UI behavior. It removes by `localId` and cannot cancel a different request.

### Runtime Parsing

`frontend/src/attachment-upload.ts::parseUploadResponse` is the only success parser:

1. Read the response body exactly once as text (bounded by the existing server response expectations) so JSON parse and schema failures are classified separately; never log the text.
2. For non-2xx, parse `UploadErrorResponse` defensively. If malformed, synthesize `upload_http` with status and local `requestId`, not raw body.
3. For 2xx, require JSON object, canonical UUID `id`, string `filename`, string `extracted_text`, finite integer `image_count >= 0`, valid compatible `images` shape if present, and `error` only null/absent.
4. Discard `extracted_text` and `images` immediately after validation; return only safe metadata `{serverId, filename, imageCount}`.
5. Reject 2xx payloads carrying a non-null error as `upload_protocol`; do not reinterpret them as a valid backend error or success.
6. Rust and TypeScript share an upload-specific JSON fixture containing current success (`error:null`), compatible success (`error` absent), structured error and malformed cases. Rust serializes/compares the authoritative success/error values; TypeScript parses the same fixture.

### Structured Errors and Logging

Frontend `UploadFailure.code` uses stable local categories where no server `ErrorCode` exists:

| Category | Trigger | Retryable default | Safe UI action |
|---|---|---:|---|
| existing server code | valid non-2xx `UploadErrorResponse` | server value | show safe server message; retry only if true |
| `upload_transport` | fetch rejects without accepted abort/timeout | true | check connection and retry |
| `upload_timeout` | deadline wins | true | retry; indicate processing timed out |
| `upload_cancelled` | explicit lifecycle abort wins | true | indicate cancelled only if chip remains |
| `upload_protocol` | content type/JSON/schema/2xx error invalid | false | reload/update client; retain trace ID |
| `upload_correlation` | valid response cannot resolve owned request | false | safe generic failure and diagnostics |
| `upload_state_conflict` | impossible reducer event/state relation | false | safe generic failure; no cross-write |

Every `UploadFailure` contains `code`, `retryable`, `operation="upload"`, `trace_id`, optional HTTP status and UUID-only `{request_id, local_id, server_id?}`. Browser sanitization remains defensive. UI title/message must never include body text, extraction details, local paths, auth query values or upstream error details.

Server logs use structured events with `request_id`, `upload_id` only after allocation, `phase`, `outcome`, `status`, `elapsed_ms`, byte counts and safe error code. They omit filenames and all content. `tracing` messages in `attachments.rs` retain their current redaction. No request header, URL query, multipart body, provider body or `safe_details` raw value is logged.

Retry/automatic recovery/degradation visibility follows the existing trace/status mechanisms: this fix does not add automatic upload retries. A user-triggered retry creates a new requestId/local lifecycle and remains visible as a new upload attempt; no hidden retry may reuse a terminal request.

### Module-Level Changes

Assuming exploratory checking confirms a boundary covered by this design:

1. **`rust/crates/cli/src/serve_http/protocol.rs`**
   - Keep `UploadResponse` as the Rust success authority, document `error=None` invariant, and add upload wire fixture assertions.
   - Do not change WebSocket tags or `AttachmentRef` compatibility.
2. **`rust/crates/cli/src/serve_http/upload_service.rs`**
   - Accept/normalize optional request identity header; create one trace per request.
   - Emit safe phase logs and attach correlation headers to success/error responses.
   - Keep processing, storage, UUID, permissions and error status mappings unchanged.
   - Add handler integration seams/tests for deterministic processor success/error; do not introduce a new production endpoint.
3. **`rust/crates/cli/src/serve_http/http_error.rs`**
   - Make serialization fallback satisfy the same structured, no-store, nosniff, traceable error contract.
   - Add response header/body contract helpers only if they remain shared and do not alter unrelated payload semantics.
4. **`frontend/src/types.ts`**
   - Separate `UploadSuccessResponse` and `UploadErrorResponse`; represent success `error` as null/absent.
   - Keep `AttachmentRef` wire shape unchanged.
5. **`frontend/src/attachment-upload.ts`** (new focused pure module)
   - Own UUID validation, response parsing, failure classification, state/event types and pure reducer.
   - Export dependency-injected request runner hooks for fetch, clock and timers; no React/store dependency.
6. **`frontend/src/store/slices.ts`**
   - Replace mutable dual-purpose attachment ID/uploading/error behavior with stable local/request/server identities and reducer-backed transitions.
   - Preserve sanitization: `extracted_text` stays empty and `images` stays absent in browser state.
7. **`frontend/src/components/InputBox.tsx`**
   - Limit responsibilities to file intake, request orchestration, in-flight controller/timer cleanup, rendering phase, removal and mapping succeeded `serverId` to existing `AttachmentRef`.
   - Preserve input/drop/paste filters, send gating, voice behavior and auth URL behavior.
8. **Shared upload fixture/tests**
   - Add fixtures only under existing Rust/frontend test locations during implementation. Do not modify lean-core spec or unrelated protocol fixtures unless the upload fixture deliberately references them.

No extraction implementation change is authorized without diagnostic evidence identifying extraction as the failing boundary. In particular, this bugfix must not change provider selection, OCR prompts, tile count, PDF sparse-text threshold, image caps, inline text cap or attachment count cap.

## Testing Strategy

### Validation Approach

Testing uses two phases. Phase A runs on unfixed code and must surface the observed counterexample or identify the first unproven boundary. Phase B runs after the minimal fix and checks both `C(X)` and `¬C(X)`. Pure state/contract tests use deterministic generation and fake time; HTTP/processor tests use committed non-sensitive fixtures and controlled upstream responses.

### Exploratory Bug Condition Checking

**Goal**: 在未修复代码上证明 PNG、Markdown、PDF 中至少一个“后端保存成功但前端失败/不终结”的反例，并把失败定位到 response build/transport/parse/correlation/terminal transition 中的第一处不变量破坏。

**Test Plan**:

1. 给三个请求分配 request ID，并采集前述 UUID-only milestone。
2. 先独立调用 handler/contract harness，证明 status、headers、完整 body 和 Rust round-trip。
3. 再通过 fake fetch 向当前前端路径注入同一 body，观察 parse 和 chip 更新。
4. 最后运行真实 browser→HTTP loopback flow；逐层比较相同 request 的证据。
5. 若未修复代码未出现反例，不改生产逻辑；转而检查实际部署 binary/assets/proxy/service worker，并更新设计。

**Test Cases**:

1. **PNG OCR counterexample**: 使用真实有效 PNG fixture 和受控 DeepSeek-compatible mock responses，保持当前 global/tile 调用语义；断言 `attachment.json` 存在后 chip 的实际终态。
2. **Markdown counterexample**: 使用 UTF-8 `.md` fixture 走直接读取，无外部 provider；这是最小确定性端到端反例。
3. **PDF counterexample**: 使用真实文本 PDF fixture（超过当前 50 个非空白字符阈值、预期 `images=0`）走 `pdftotext/pdfimages`；记录保存与 browser settlement。
4. **Delayed response**: handler 已保存后延迟返回 200，分别在 deadline 前后完成，确认现有/修复后行为边界。
5. **Deployment skew**: 向 parser 注入 `error:null`、缺省、非空、HTML、空 body 和错误字段类型，确认具体拒绝点。

**Expected Counterexamples**:

- handler 有 `metadata_stored`，但没有可匹配的 browser status/body；
- browser 收到 200，但 JSON/schema 校验失败；
- success 已解析，但临时 ID 更新未命中或 generation 已失效；
- timeout/abort/旧闭包先提交失败终态；
- 若上述均不存在，则现有证据反驳假设，需要回到诊断而非实施猜测性修复。

### Fix Checking

**Goal**: 对所有满足 bug condition 的输入验证修复函数产生匹配成功。

```pascal
FOR ALL input WHERE isBugCondition(input) DO
  result := processAttachment_fixed(input)
  ASSERT expectedBehavior(result)
END FOR
```

实现方式：

- Rust 生成合法 `UploadResponse` 记录并进行 value→JSON bytes→value round-trip；handler integration 断言 200、headers、完整 body 和 persisted ID 一致。
- TypeScript 对共享 fixture 和生成记录运行 `parseUploadResponse`，再把 success event 交给纯 reducer。
- 生成请求数量 1..6、不同 completion 顺序和 fake delays；每个 persisted success 必须归属原 request/local chip。
- PNG/Markdown/PDF 三个真实 fixture 都要跨越“处理→保存→HTTP→parser→reducer→AttachmentRef”边界，不能只测 processor 日志。

### Preservation Checking

**Goal**: 对所有不满足 bug condition 的输入验证修复前后规范化观察一致。

```pascal
FOR ALL input WHERE NOT isBugCondition(input) DO
  ASSERT normalize(processAttachment_original(input))
       = normalize(processAttachment_fixed(input))
END FOR
```

**Testing Approach**:

- 在修复前保存 golden observations：接受/拒绝、status/code/retryable、提取文本 hash/字符数、image count、provider request count/shape、AttachmentRef 和最终 prompt block shape。
- 不把附件正文写入 snapshot/log；测试内比较内容后仅报告 hash/长度。
- 规范化 UUID、trace ID、时间戳、elapsed time 和 JSON object order；不得规范化 status、error code、retryability、filename、文本、图片数量、引用 ID 所指向的内容或安全决定。
- 对真实失败矩阵和非附件行为运行差分测试；前端入口代码未改部分仍由 build/type check 和 focused characterization 覆盖。

### Unit Tests

#### Rust

- `protocol.rs`: success response `error:null` 序列化、字段名/type、UUID/filename/image_count round-trip。
- `http_error.rs`: 结构化错误与 serialization fallback 的 status/headers/body/trace/redaction。
- `upload_service.rs`: request header 规范化、response correlation headers、规范 UUID、private storage 和既有错误映射。
- `attachments.rs`: PNG mock OCR、Markdown direct read、PDF text extraction fixture 的当前 golden output；不改变 OCR/provider semantics。
- `run_handler.rs`: succeeded server ID 加载 `attachment.json` 并保持文本/图片注入，legacy fallback 继续受限。

#### TypeScript

- `attachment-upload.test.ts`: success/error parser、null/absent error、malformed JSON、wrong field types、noncanonical UUID、content type 和 non-null success error。
- reducer tests: pending→uploading→success/failed；duplicate/late no-op；unknown request；stale generation；submit mapping。
- fake clock tests: success-before-timeout、timeout-before-success、abort-before-success、cleanup abort、timer exactly-once clear。
- `security.test.ts`: structured errors and attachment metadata sanitization; generated secret/path/content never appears in state or diagnostic output。

### Property-Based Tests

不新增运行时依赖。实现阶段优先使用仓库现有 Node/Rust test harness 中的确定性生成器；若后续任务选择第三方测试库，必须固定精确版本并单独说明理由。

1. **Wire generator**
   - Rust 生成 Unicode-safe filenames、canonical UUIDs、`image_count` 边界和 optional compatible fields，写入 JSON；TypeScript parser 验证 Property 3。
   - 负生成器逐字段删除、换 type、设置负数/小数/NaN 表达、非规范 UUID、non-null error 和 trailing/malformed JSON，必须按 `upload_protocol` 拒绝。
2. **Event-sequence generator**
   - 使用固定 seed 的小型 LCG 生成 1..6 个 request 及 start/success/failure/timeout/abort/duplicate/invalidate 事件；另对每请求最多三个关键事件做小规模穷举 interleaving。
   - 每步断言 terminal count ≤ 1、其他 owner state 不变、serverId 只来自本 request 的 valid success、终态不可降级，验证 Property 4。
3. **Error-redaction generator**
   - 将路径、Bearer、`sk-*`、authorization、attachment text/base64 标记嵌入内部错误和 safe details；外部 JSON、browser failure、logs capture 均不得包含标记，验证 Property 5。
4. **Differential preservation generator**
   - 生成扩展名/signature/size/auth/config/storage/extraction 结果组合，比较修复前 characterization adapter 与新 adapter 的规范化观察，验证 Property 2。

生成测试必须打印 seed 和最小化后的事件序列/字段组合，不打印文件内容或秘密。固定 regression seeds 与发现的最小反例一起提交。

### Integration Tests

1. **HTTP Markdown fixture**: loopback Axum router + real multipart request；验证 private write、200 headers/body、TS-compatible fixture 和后续 stored reference injection。
2. **PNG OCR fixture**: mock upstream server 返回确定 OCR JSON；真实 PNG 走当前 decode/tile/provider path，验证 processor output、保存、HTTP response 和前端 success。
3. **PDF fixture**: committed real PDF，环境具备 `pdftotext`/`pdfimages`；验证当前文本 golden、`image_count=0`、HTTP success 和注入。CI 缺少 poppler 应视为环境配置失败，不得悄悄改走其他语义。
4. **Real errors**: unauthorized、invalid doc config、oversize、unsupported/signature mismatch、storage failure、extractor failure、interrupted stream，断言非 2xx structured error 和安全字段。
5. **Malformed browser responses**: fake fetch 返回 HTML、empty/truncated JSON、wrong schema、unknown server ID，断言匹配请求失败且其他附件不变。
6. **Concurrent attachments**: Markdown/PNG/PDF 同时开始，按全部六种 completion order 和重复 completion 验证 stable ownership。
7. **Timeout/abort/late response**: fake clock 精确推进到 deadline 前后；验证 first accepted terminal wins，controller abort once，late result no-op。
8. **UI regression**: chip 分别渲染 pending/uploading spinner、succeeded check、failed red X；成功不显示红 X，失败显示 safe title；send gating、remove 和多附件顺序保持。
9. **Session boundary**: 上传中切换 session/卸载，旧请求被 abort/invalidated，晚到成功不恢复 chip、不写新 session。

### Requirement Traceability

| Requirements | Design/verification owner |
|---|---|
| 1.1–1.3 | Current sequence, concrete examples, exploratory PNG/Markdown/PDF fixtures |
| 1.4–1.5 | HTTP contract, root-cause evidence, runtime parser, wire fixtures |
| 1.6 | Three-ID ownership, pure state machine, event interleaving tests |
| 1.7 | Structured error taxonomy, UUID-only milestones, redaction properties |
| 1.8 | Unit/property/integration suites and commands below |
| 2.1–2.3 | Property 1 plus three end-to-end fixture tests |
| 2.4 | Handler success/error contract and Rust response tests |
| 2.5 | `parseUploadResponse`, shared fixtures, Property 3 |
| 2.6 | reducer first-terminal rules, AbortController registry, Property 4 |
| 2.7 | UploadFailure/AppError mapping, logging policy, Property 5 |
| 2.8 | Full validation matrix |
| 3.1–3.3 | unchanged UI inputs, extraction golden tests, stored-reference injection/security |
| 3.4–3.5 | real-error matrix and existing security tests |
| 3.6 | generation invalidation/session boundary tests |
| 3.7 | upload fixture plus existing Rust/TypeScript protocol fixtures |
| 3.8 | Property 2 differential preservation and full checks |

### Validation Commands

后续 tasks 应从仓库指定目录执行以下非 watch 命令：

```bash
# Focused Rust tests (workspace: /home/baohx/NonoClaw/rust)
cargo test -p nonoclaw serve_http::upload_service --locked
cargo test -p nonoclaw serve_http::http_error --locked
cargo test -p nonoclaw serve_http::protocol --locked
cargo test -p nonoclaw attachments --locked
cargo test -p nonoclaw serve_http::run_handler --locked

# Full Rust quality gates
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings

# Frontend focused tests and production build (workspace: /home/baohx/NonoClaw/frontend)
npm run test:upload
npm run test:transitions
npm run test:security
npm run build
```

实现 task 需要在 `frontend/package.json` 增加非 watch 的 `test:upload` script，调用 Node 单次执行的 upload parser/reducer test。PDF integration 前置检查为系统可执行文件 `pdftotext` 和 `pdfimages` 可用；PNG OCR integration 使用本地 mock，不访问真实第三方 API，不传输项目代码、秘密或用户附件。
