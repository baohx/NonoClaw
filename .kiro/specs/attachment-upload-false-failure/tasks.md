# Implementation Plan

> 执行约束：严格按顺序执行。除测试/诊断接缝外，在任务 1 取得未修复链路反例并定位首个失效边界前，不得修改生产逻辑。不得重写 OCR/PDF 算法、改变 provider/提取阈值、增加或删除格式、放宽鉴权/32 MiB/签名/UUID/路径/私有存储边界，也不得修改 requirements、design、`.config.kiro` 或 lean-core spec。所有测试命令必须单次退出，禁止 watch 模式。

- [ ] 1. 编写并运行未修复链路的最小反例探索测试
  - **Property 1: Bug Condition** - Persisted Upload Settles as Matching Success
  - **CRITICAL**: 在任何生产修复前完成；该性质在未修复代码上必须失败，失败才确认 bug condition 存在。不得因预期失败而修改断言或立即修生产代码。
  - **主要模块**: `rust/crates/cli/src/serve_http/upload_service.rs` 的测试接缝、`frontend/src/components/InputBox.tsx` 的现有上传链路测试接缝、拟新增的聚焦上传测试、无敏感测试 fixture。
  - 以设计中的 `isBugCondition(X)` 限定输入：`backend_success=true`、持久化 `attachment.json` 存在、成功前无明确 abort、无独立确认的传输失败，但前端进入 `failed`，或在确定 settlement deadline 后仍为 `pending/uploading`。
  - 先构造最小确定性 Markdown 反例：真实 UTF-8 `.md` 经现有直接读取与保存路径完成，再跨越 handler response、浏览器 decode、chip 更新；随后以受控本地 OCR mock 的 PNG 和真实文本 PDF fixture 复核同一性质。不得调用真实第三方 API；PDF 环境缺少 `pdftotext`/`pdfimages` 时报告环境失败，不得改走另一算法。
  - 断言设计中的 `expectedBehavior(result)`：2xx `application/json`、有效成功 payload、匹配请求进入一次 `succeeded`、前端 `serverId` 等于持久化 ID、无红色 X；在未修复链路上记录首个失败断言和最小反例。
  - 分层运行 handler contract、向现有前端路径注入相同 synthetic wire body、真实 browser→loopback HTTP 链路，定位 response build/transport/parse/correlation/terminal transition 中最先破坏的不变量。
  - 加入安全诊断断言：只允许 UUID、阶段、status、body byte length、elapsed、类别和部署版本；不得记录文件名、路径、正文、图片/base64、token、URL query、原始请求/响应体或上游私密错误。不得提交含 auth token/附件内容的 HAR。
  - 生成测试采用固定 seed，输出 seed 与最小化后的事件/字段反例，不输出内容或秘密；对确定性现场 bug 将性质范围缩到已确认的具体 fixture/事件序列。
  - **完成条件**: 未修复代码上至少一个测试按预期失败，并留下可复现命令、固定 seed、最小反例和首个失效边界；测试代码本身可单独运行。
  - **停止门槛**: 若 Markdown、PNG、PDF 的 handler、wire、parser、correlation 与 loopback 链路在未修复版本上全部通过，停止所有猜测性生产修改；仅保留诊断/characterization 测试，转而收集同一 `requestId` 的部署 binary/asset 版本、proxy/service-worker、status/headers/body length/transfer timing 证据，并先更新需求/设计后再恢复实现。不得宣称修复完成。
  - _Design: Bug Condition, Root-Cause Evidence Required, Ranked Hypotheses, Exploratory Bug Condition Checking_
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8_

- [ ] 2. 编写并运行未修复行为的 preservation 性质测试
  - **Property 2: Preservation** - Non-Bug Upload Behavior and Security
  - **IMPORTANT**: 采用 observation-first；必须在生产修复前观察并固化 `NOT isBugCondition(X)` 的实际行为，测试在未修复代码上必须通过。
  - **主要模块**: `rust/crates/cli/src/attachments.rs`、`rust/crates/cli/src/serve_http/{upload_service,run_handler,http_error}.rs`、`frontend/src/{security.test.ts,store/transitions.test.ts}` 及聚焦 characterization adapters。
  - 保存 PNG mock OCR、Markdown direct read、文本 PDF 的基线观察：接受/拒绝、status/code/retryable、提取内容 hash/字符数、`image_count`、provider 请求次数/形状、持久化元数据、`AttachmentRef` 和最终 prompt block 形状；正文只在测试内比较，不进入 snapshot/log。
  - 对真实失败与安全矩阵建立基线：unauthorized、无效配置、超限、不支持、扩展名/签名不匹配、存储/提取失败、独立网络失败、成功前明确取消，以及文件名净化、规范 UUID、私有权限、canonical path/traversal 拒绝。
  - 覆盖既有选择/拖放/粘贴、多附件、处理中反馈、成功/失败显示、移除、提交、legacy inline fallback、session 切换/清理/断线重连和旧 generation 丢弃；不得增加格式或改变 OCR/PDF/provider 语义。
  - 使用固定 seed 的差分生成器覆盖扩展名/signature/size/auth/config/storage/extraction 组合，比较修复前 characterization 观察；只规范化 UUID、trace ID、时间戳、elapsed 和 JSON 字段顺序，不得规范化 status、code、retryable、文本、图片数、引用内容或安全决定。
  - **完成条件**: preservation 测试可独立执行并在未修复代码上通过；固定 seed、golden hash/长度及真实失败分类均已记录且不含敏感数据。
  - _Design: Preservation Requirements, Property 2, Preservation Checking_
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8_

- [ ] 3. 修复 attachment upload 假失败并加固跨边界契约
  - **依赖**: 仅当任务 1 已取得反例并通过诊断门槛、任务 2 已建立通过的 preservation baseline 后执行。
  - **主要模块**: Rust upload HTTP 边界、共享 wire fixture、TypeScript parser/state machine、`InputBox` I/O 编排和 UI。
  - **完成条件**: 3.1–3.10 全部完成；最小生产修改与任务 1 定位的边界一致，且未触碰禁止范围。
  - _Bug_Condition: `isBugCondition(input)` from design_
  - _Expected_Behavior: `expectedBehavior(result)` from design_
  - _Preservation: Preservation Requirements from design_
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.7, 2.8, 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8_

  - [ ] 3.1 实现 Rust 上传成功/错误契约与 request correlation
    - **主要模块**: `rust/crates/cli/src/serve_http/{protocol,upload_service,http_error}.rs`。
    - 保持 `/api/upload`、multipart `file`、可选 query token 和成功 HTTP 200；使持久化结果 ID 与 `UploadResponse.id` 一致，成功响应为完整 `application/json`，保留 `no-store`/`nosniff`，且 `error` 仅为 `null`。
    - 对 response 序列化失败返回非 2xx `UploadErrorResponse`；统一正常错误与 fallback 的 `error/code/retryable/operation/trace_id/safe_details`、trace header 和安全 headers，不泄露原始序列化错误。
    - 接受可选 `x-upload-request-id`：只反射规范 UUID；缺省/无效时为兼容旧客户端生成新 correlation UUID，不削弱鉴权、不拒绝 legacy client。生成独立 `serverId`，返回 `x-upload-request-id`/`x-trace-id` 并用 UUID-only phase logs 串联 `received/stored/extracted/metadata_stored/response_built`。
    - 保持现有 auth/config/size/type/signature/storage/extraction status/code 映射、私有权限和附件处理代码不变。
    - **完成条件**: handler 单元/集成测试证明成功 body 可 round-trip、ID 与持久化结果一致、错误 envelope/headers 完整且脱敏、合法/缺省/无效 request header 行为确定；聚焦 Rust 测试通过。
    - _Bug_Condition: persisted backend success without a matching frontend success_
    - _Expected_Behavior: valid complete 200 JSON response with correlated canonical IDs_
    - _Preservation: existing endpoint, processing, security and real-error mappings_
    - _Requirements: 2.4, 2.7, 2.8, 3.2, 3.4, 3.5, 3.7, 3.8_

  - [ ] 3.2 建立 Rust/TypeScript 共用 upload wire fixture
    - **主要模块**: `rust/crates/cli/src/serve_http/protocol.rs` 测试、现有 Rust/frontend 测试目录中的单一 upload-specific JSON fixture、`frontend/package.json`。
    - fixture 覆盖当前成功 `error:null`、兼容成功 `error` 缺省、结构化非 2xx 错误，以及 malformed/缺字段/错类型/非规范 UUID/负数或小数 `image_count`/2xx non-null error；只用 synthetic 无敏感值。
    - Rust 从权威类型序列化并核对字段/类型/headers；TypeScript 消费同一 fixture，禁止复制出第二套漂移 fixture。不得修改既有 WebSocket protocol fixture 或 lean-core spec。
    - 增加非 watch 的 `npm run test:upload`，使用仓库现有 Node 单次执行 harness；不新增运行时依赖。
    - **完成条件**: Rust 与 TypeScript 可分别单独运行同一 fixture 的契约测试，成功/错误兼容语义一致，负例分类稳定。
    - _Bug_Condition: valid backend success may be lost across serialization/decode boundaries_
    - _Expected_Behavior: semantic wire round-trip preserves required upload fields_
    - _Preservation: current compatible fields and unrelated protocol mappings remain unchanged_
    - _Requirements: 1.4, 1.5, 1.8, 2.4, 2.5, 2.8, 3.7, 3.8_

  - [ ] 3.3 实现唯一 TypeScript runtime response parser
    - **Property 3: Success Wire Contract** - Rust/TypeScript Semantic Round Trip
    - **主要模块**: 新建 `frontend/src/attachment-upload.ts`、`frontend/src/types.ts`、`frontend/src/attachment-upload.test.ts`。
    - 分离 `UploadSuccessResponse`/`UploadErrorResponse` 类型；`parseUploadResponse` 只读取 body 一次且不记录 body，区分非 2xx、content type、JSON 与 schema 失败。
    - 成功只接受对象、规范 lowercase hyphenated UUID、string filename/extracted_text、有限非负整数 `image_count`、合法兼容 images、`error:null`/缺省；2xx non-null error 和所有 malformed schema 归类 `upload_protocol`。
    - 验证后立即丢弃 `extracted_text/images`，仅返回 `{serverId, filename, imageCount}`；非 2xx defensively 解析结构化错误，malformed error 只保留 status、local request correlation 和安全本地分类。
    - 用固定 seed 生成 Unicode-safe filename、UUID、计数边界和逐字段负变异；输出 seed/最小字段组合，不输出 payload 内容。
    - **完成条件**: parser/Property 3 单测覆盖 null/absent error、malformed JSON/schema、content type、非规范 ID 与 structured error，并由 `npm run test:upload` 单次通过。
    - _Bug_Condition: browser cannot deterministically validate a successful persisted response_
    - _Expected_Behavior: one parse plus schema validation yields safe correlated metadata_
    - _Preservation: compatible response fields remain accepted; content stays server-private_
    - _Requirements: 1.5, 1.7, 1.8, 2.5, 2.7, 2.8, 3.3, 3.7, 3.8_

  - [ ] 3.4 实现 requestId/localId/serverId 模型和纯单终态 reducer
    - **Property 4: Correlation** - Deterministic Single-Terminal Isolation
    - **主要模块**: `frontend/src/attachment-upload.ts`、`frontend/src/store/slices.ts`、对应 reducer/property tests。
    - `requestId` 唯一标识上传操作，`localId` 稳定标识 chip/React key/remove，`serverId` 仅在成功校验后绑定并仅用于 `AttachmentRef.id`；禁止用 filename、数组 index、完成顺序或未验证 server ID 查找 owner。
    - 纯 reducer 按 request ownership、expected localId、session generation 和 phase 接受首个 terminal event；返回可诊断 disposition。accepted success 原子写入 server metadata 并清错误；failure 不写 `serverId`；所有 duplicate/late/stale/unknown/cross-owner 事件为隔离的 no-op。
    - 用固定 seed LCG 生成 1..6 个请求和 start/success/failure/timeout/abort/duplicate/invalidate interleaving，并小规模穷举每请求关键事件；每步断言 terminal count ≤ 1、其他 owner 不变、serverId 只来自所属 success、终态不可降级。固定发现的最小 regression seed。
    - 保持浏览器 state 中 `extracted_text=""`、无 images/正文；成功提交只映射 `serverId`，失败/处理中不进入 attachments。
    - **完成条件**: reducer 是唯一终态写入者；单请求、六种三附件 completion order、重复/乱序、未知 ID、stale generation 和跨 owner tests 确定性通过。
    - _Bug_Condition: asynchronous updates can fail to settle or correlate the persisted success_
    - _Expected_Behavior: each request mutates only its local owner and reaches at most one terminal state_
    - _Preservation: submit mapping, sanitization and session isolation remain compatible_
    - _Requirements: 1.6, 1.8, 2.2, 2.3, 2.6, 2.8, 3.1, 3.3, 3.6, 3.8_

  - [ ] 3.5 将 InputBox 限定为可测试的 fetch/timeout/session 编排层
    - **主要模块**: `frontend/src/components/InputBox.tsx`、`frontend/src/attachment-upload.ts` 的 request runner/injected dependencies、相关 fake-clock tests。
    - 每个上传在加入 chip 前生成独立 `requestId/localId`，发送 `x-upload-request-id`，通过可注入 fetch/clock/setTimer/clearTimer 管理独立 `AbortController` 与 in-flight registry。
    - 从 reducer 接受 `start` 后计算固定 120,000 ms deadline；success/failure/timeout/abort 只清 timer 和 registry 一次。success 先被接受时晚 timeout/abort 不得降级；timeout/abort 先接受时 abort controller 一次，晚 response/parse completion 不得写任何 chip。
    - session 切换、清理和 unmount 先失效 generation，再 abort/clear 全部 in-flight；catch/晚结果不得恢复旧附件或污染新 session。用户重试创建新 lifecycle，不做隐藏自动重试。
    - 保持选择/拖放/粘贴 allowlist、并发多附件、auth URL、voice、send gating 和 terminal-only remove 行为；不得增加格式。
    - **完成条件**: fake clock 无真实等待地覆盖 deadline 前/后、success-vs-timeout、abort、cleanup、晚到结果和 timer/controller exactly-once；`InputBox` 不再有 ad hoc terminal state 写入。
    - _Bug_Condition: valid requests may be settled by stale closures, timeout, abort or wrong identity_
    - _Expected_Behavior: accepted first terminal event wins within injectable 120s policy_
    - _Preservation: file intake, auth, voice, send/remove and session cleanup semantics_
    - _Requirements: 1.6, 1.7, 1.8, 2.1, 2.2, 2.3, 2.6, 2.8, 3.1, 3.6, 3.8_

  - [ ] 3.6 实现结构化前端错误、安全诊断与 UI 终态
    - **Property 5: Failure Contract** - Structured and Redacted Diagnostics
    - **主要模块**: `frontend/src/attachment-upload.ts`、`frontend/src/components/InputBox.tsx`、`frontend/src/security.test.ts` 及 attachment chip rendering tests。
    - 映射 server error 与本地 `upload_transport/upload_timeout/upload_cancelled/upload_protocol/upload_correlation/upload_state_conflict`；每项包含 `code/retryable/operation/trace_id`、可选 status 和 UUID-only correlation，选择类别一致且可操作的安全 UI message。
    - UI 明确渲染 pending/uploading spinner、succeeded check、failed red X；成功永不显示红 X，失败 title/message 不含 body、提取详情、路径、query/auth 或 upstream detail。重试/取消/降级依既有 trace/status 机制可见，不做隐藏 retry。
    - 以固定 seed 将 path、Bearer、`sk-*`、authorization、attachment text/base64 marker 注入内部错误/safe details，断言 server JSON、browser state、UI 与 captured logs 均不含 marker。
    - **完成条件**: 每种失败类别均有确定 UI/`retryable` 行为，Property 5 与现有 security tests 单次通过，真实错误不会被误标成功。
    - _Bug_Condition: a red X lacks a trustworthy boundary-specific diagnosis_
    - _Expected_Behavior: every real failure ends once with a safe distinguishable category_
    - _Preservation: private data boundaries and existing safe server errors_
    - _Requirements: 1.7, 1.8, 2.7, 2.8, 3.3, 3.4, 3.5, 3.8_

  - [ ] 3.7 编写 PNG、Markdown、PDF 跨边界 E2E 与消息注入回归
    - **主要模块**: Rust loopback upload integration tests、frontend upload integration/UI tests、`rust/crates/cli/src/serve_http/run_handler.rs` 测试。
    - 对 Markdown 使用真实 multipart direct-read fixture；PNG 使用本地 mock upstream 并保持当前 decode/tile/provider 请求语义；PDF 使用真实文本 PDF 并要求 `pdftotext`/`pdfimages`，保持当前文本 golden 与 `image_count=0`。
    - 每个 fixture 跨越处理→私有保存→HTTP 200/headers/body→TS parser→reducer→成功 chip→`AttachmentRef.serverId`→现有 stored-reference prompt/image injection；断言成功在 deadline 内单终态且无红 X。
    - 三附件并发执行全部六种 completion order、重复 completion 和延迟 response；验证 ownership、顺序、send gating 和 late no-op。PDF 工具缺失应明确失败并给出环境前置条件，不得 fallback 到新语义。
    - **完成条件**: PNG/MD/PDF 可分别聚焦运行也可 Run All；输出只含安全 hash/长度/计数，提取 golden、provider 形状和 legacy inline fallback 与基线一致。
    - _Bug_Condition: persisted PNG/Markdown/PDF results end as failed or unsettled chips_
    - _Expected_Behavior: each persisted fixture becomes its matching usable AttachmentRef_
    - _Preservation: existing OCR/PDF/text extraction and message injection semantics_
    - _Requirements: 1.1, 1.2, 1.3, 1.8, 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.8, 3.2, 3.3, 3.8_

  - [ ] 3.8 验证真实失败、安全边界与 session preservation
    - **主要模块**: `rust/crates/cli/src/serve_http/{upload_service,http_error,run_handler}.rs` tests、`frontend/src/{attachment-upload.test.ts,security.test.ts}`、session/UI integration tests。
    - 覆盖 unauthorized、无效 doc config、oversize、不支持/签名不匹配、存储/提取失败、interrupted stream、malformed browser responses、未知/不匹配 ID、明确取消和 session invalidation；断言非 2xx/对应本地失败、structured safe envelope、正确 retryability、其他附件不变。
    - 重跑 32 MiB、filename sanitize、canonical UUID、`0700/0600`、canonical path/traversal 和浏览器不持久化正文/图片的安全检查；确认旧 session 晚结果不能恢复 chip。
    - 比较任务 2 baseline，仅规范化获准的 nondeterministic fields；任何 extraction/provider/security/error classification 差异都视为回归，不得用更新 golden 掩盖。
    - **完成条件**: 真实失败矩阵全部仍失败且安全分类不变；安全与 session tests 通过；无新增格式、算法或放宽边界。
    - _Bug_Condition: excludes confirmed backend/transport/cancel/security failures_
    - _Expected_Behavior: only valid persisted successes settle as success_
    - _Preservation: all real failures, security decisions and session effects_
    - _Requirements: 2.7, 2.8, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8_

  - [ ] 3.9 重新运行同一 bug condition 探索性质并验证修复
    - **Property 1: Expected Behavior** - Persisted Upload Settles as Matching Success
    - **IMPORTANT**: 只重跑任务 1 的同一测试与固定最小反例，不另写放宽版测试。
    - **主要模块**: 任务 1 的 exploration harness 及本次最小修复模块。
    - 对每个满足 `isBugCondition(X)` 的已确认输入断言 `expectedBehavior(result)`：有效 2xx JSON、匹配 `serverId`、一次 succeeded、无失败指示；保留反例 seed 作为 regression。
    - **完成条件**: 任务 1 原先预期失败的测试现在通过，且 failure boundary 的修复证据与诊断结论一致。
    - _Bug_Condition: `isBugCondition(input)` from design_
    - _Expected_Behavior: `expectedBehavior(result)` from design_
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.8_

  - [ ] 3.10 重新运行同一 preservation 性质并验证无回归
    - **Property 2: Preservation** - Non-Bug Upload Behavior and Security
    - **IMPORTANT**: 只重跑任务 2 的同一 observation-first tests、golden 和固定 seed；不得为通过修复而重写 baseline。
    - **主要模块**: 任务 2 的 characterization/differential harness 与全部受影响模块。
    - 比较修复前后 normalized acceptance/rejection、提取结果、image count、provider shape、AttachmentRef、prompt 注入、真实错误、安全与 session effect。
    - **完成条件**: 所有 `NOT isBugCondition(X)` 的 preservation tests 继续通过；差异仅限设计允许的 UUID-only 诊断、稳定 local identity、明确错误类别和规范化字段。
    - _Preservation: Preservation Requirements and Property 2 from design_
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8_

- [ ] 4. Checkpoint - 运行 focused 与 full validation
  - **主要模块**: `/home/baohx/NonoClaw/rust` workspace、`/home/baohx/NonoClaw/frontend` package 和全部新增/既有测试。
  - 先运行聚焦 Rust 单次测试：
    - `cargo test -p nonoclaw serve_http::upload_service --locked`
    - `cargo test -p nonoclaw serve_http::http_error --locked`
    - `cargo test -p nonoclaw serve_http::protocol --locked`
    - `cargo test -p nonoclaw attachments --locked`
    - `cargo test -p nonoclaw serve_http::run_handler --locked`
  - 再运行前端非 watch 聚焦检查：`npm run test:upload`、`npm run test:transitions`、`npm run test:security`、`npm run build`。
  - 最后运行 Rust full gates：`cargo fmt --all -- --check`、`cargo test --workspace --locked`、`cargo clippy --workspace --all-targets --locked -- -D warnings`。
  - 确认每个 deterministic property test 报告固定 seed；确认 PNG mock 不联网，PDF 工具前置满足；确认没有 watch/dev/preview 进程、未修改禁止文件、未新增格式或依赖。
  - **完成条件**: focused 与 full commands 全部退出码为 0，Property 1 修复检查通过、Property 2 preservation 通过、Properties 3–5 通过；若失败，修复原因后重跑最小受影响集合及最终 full gates。
  - _Requirements: 1.8, 2.8, 3.8_
