# Bugfix Requirements Document

## Introduction

本修复解决 Web UI 附件上传的“后端成功、前端失败”假失败。已确认的事实仅包括：PNG 已由 DeepSeek OCR 成功提取（约 10 秒，`pages=1 chars=503`）、Markdown 已进入 `text/markdown` 读取路径、PDF 已成功完成文本与图片提取（约 2 秒，`chars=15967 images=0`），三个请求的后端处理均未报告异常；但前端长时间保持处理中，随后在文件名前显示红色 X。现有证据尚不能确定问题发生在 HTTP 响应、序列化、浏览器解析、attachment ID 关联、异步状态转换还是 timeout/abort/race，因此本需求不预设根因。

本修复须兼容 lean-core Requirement 2.5（同一事件类型和单一权威映射）、8.6（ClientMsg、ServerMsg 与 EngineEvent 协议一致性检查）、8.8（上传失败返回结构化、可恢复且不泄露敏感路径的错误）、9.5（重试、自动修复与降级行为可见）、11.6（Rust 格式化、workspace tests、Clippy 与前端 production build）以及 11.7（聚焦契约测试）。

## Bug Analysis

### Current Behavior (Defect)

下列条款描述已观察到的缺陷及尚未被验证的跨边界不变量；“尚未被验证”不表示对应环节已被认定为根因。

1.1 WHEN 后端成功完成 PNG OCR、产生并保存可引用的附件结果且未报告处理异常 THEN the system 长时间保持该附件为处理中，随后在文件名前显示红色 X

1.2 WHEN 后端成功读取纯文本 Markdown、产生并保存可引用的附件结果且未报告处理异常 THEN the system 长时间保持该附件为处理中，随后在文件名前显示红色 X

1.3 WHEN 后端成功完成 PDF 文本与图片提取、产生并保存可引用的附件结果且未报告处理异常 THEN the system 长时间保持该附件为处理中，随后在文件名前显示红色 X

1.4 WHEN 后端处理与附件结果保存均成功、请求在成功完成前未被明确取消且不存在独立确认的外部传输中断 THEN the system 当前不能保证 HTTP 上传结果以非错误 2xx JSON 成功响应完整结束，也不能从现有证据确定响应生成、序列化或传输是否造成了假失败

1.5 WHEN 独立契约校验确认 HTTP 响应体是有效的上传成功 payload THEN the system 当前不能保证浏览器只按成功 schema 解析该 payload，也不能保证 `error` 为 `null` 或缺省时不会被误判为失败

1.6 WHEN 一个或多个仍然有效的上传请求收到成功 payload、重复或乱序完成、timeout、abort 或其他异步更新 THEN the system 当前不能保证临时附件 ID、服务端 attachment ID 与请求身份被正确关联，也不能保证每个请求只产生一个确定且不串写其他附件的前端终态

1.7 WHEN 前端显示上传失败或超时而后端没有对应处理失败 THEN the system 当前呈现的红色 X 及可用诊断不能可靠区分后端处理失败、HTTP/传输失败、响应序列化失败、payload 解析/校验失败、ID 关联失败、timeout、abort 和状态竞争

1.8 WHEN 附件成功路径或异步终态逻辑发生回归 THEN the system 当前缺少同时覆盖 PNG OCR、纯文本 Markdown、PDF 提取、HTTP 成功契约、序列化/解析、ID 关联、并发与 timeout/abort/race 的可执行跨边界回归和生成式性质检查

用于界定本次假失败的 bug condition 为：

```pascal
FUNCTION isBugCondition(X)
  INPUT: X of type AttachmentUploadTrace
  OUTPUT: boolean

  // backend_success 表示处理成功且可引用结果已保存，由服务端事实独立判定。
  // external_transport_failure 与 explicit_abort_before_success 必须由测试注入或底层事实判定，
  // 不得仅因前端显示失败而推断为 true。
  // settlement_deadline 是产品配置或测试时钟中的确定阈值，不是人工观察时长。
  RETURN X.backend_success = true
     AND X.backend_attachment_result_exists = true
     AND X.explicit_abort_before_success = false
     AND X.external_transport_failure = false
     AND (
       X.frontend_terminal_state = failed
       OR (
         X.frontend_state IN {pending, uploading}
         AND X.settlement_deadline_elapsed = true
       )
     )
END FUNCTION
```

该条件有意不指定 HTTP、序列化、解析、关联或 race 中的某一项为根因；这些环节是同一端到端性质的待验证边界。真实后端失败、成功前已生效的用户取消以及独立确认的外部传输中断不属于 `C(X)`，其行为由回归保护条款约束。

### Expected Behavior (Correct)

2.1 WHEN 后端成功完成 PNG OCR、产生并保存可引用的附件结果且未报告处理异常 THEN the system SHALL 在确定的请求完成时限内将匹配附件从处理中转换为成功终态，显示成功状态且不显示红色 X

2.2 WHEN 后端成功读取纯文本 Markdown、产生并保存可引用的附件结果且未报告处理异常 THEN the system SHALL 在确定的请求完成时限内将匹配附件转换为成功终态，并允许其作为附件引用随消息提交

2.3 WHEN 后端成功完成 PDF 文本与图片提取、产生并保存可引用的附件结果且未报告处理异常 THEN the system SHALL 在确定的请求完成时限内将匹配附件转换为成功终态，并允许提取结果按既有附件注入流程使用

2.4 WHEN 后端处理与附件结果保存均成功、请求在成功完成前未被明确取消且不存在独立确认的外部传输中断 THEN the system SHALL 以非错误 2xx、`application/json` 响应完整结束上传请求；成功 payload SHALL 满足兼容 schema，至少包含与已保存结果相同的规范 attachment `id`、`filename`、非负 `image_count` 和既有兼容字段，且不得同时携带非空错误；任何响应生成或序列化失败 SHALL 作为真实的结构化错误返回而不得伪装成成功或仅由前端超时推断

2.5 WHEN 独立契约校验确认 HTTP 响应体是有效的上传成功 payload THEN the system SHALL 对其完成一次确定的 JSON 解析与 schema 校验，保持 `id`、`filename`、`image_count` 及兼容字段在序列化往返后语义不变，并将 `error` 为 `null` 或缺省解释为无错误；畸形 JSON、缺少必需字段、字段类型错误或非规范 ID SHALL 进入明确的协议/解析失败而不得进入成功状态

2.6 WHEN 一个或多个仍然有效的上传请求收到成功 payload、重复或乱序完成、timeout、abort 或其他异步更新 THEN the system SHALL 以请求身份将临时附件 ID 原子替换或映射为 payload 中匹配的服务端 attachment ID，并为每个请求提交至多一个终态；成功在 timeout/abort 生效前被接受时，任何晚到 timeout、abort、重复完成或旧闭包更新不得把成功降级为失败；timeout/abort 先被接受时，晚到结果 SHALL 按确定规则忽略或协调且不得更新其他附件；完成顺序、数组 index 和其他附件的 ID 均不得用于错误关联

2.7 WHEN 上传真实地因后端处理、HTTP/传输、响应序列化、payload 解析/校验、ID 关联、timeout、abort 或状态竞争失败 THEN the system SHALL 以安全且可区分的错误类别结束匹配请求，并提供 `code`、`retryable`、`operation`、`trace_id` 及必要的脱敏关联信息；用户 SHALL 获得安全、可操作且与类别一致的反馈，重试、自动修复或降级 SHALL 依 lean-core Requirement 9.5 留下可见记录，且错误不得泄露敏感路径、附件原文、图片数据、凭据或上游私密错误

2.8 WHEN 执行本修复的验证套件 THEN the system SHALL 通过后端单元测试、HTTP/协议集成测试、前端状态转换测试和 UI 回归测试，至少覆盖 PNG OCR 成功、纯文本 Markdown 成功、PDF 提取成功、成功响应序列化往返、`error` 为 `null`/缺省、真实错误、畸形 JSON/schema、未知或不匹配 ID、临时 ID 到服务端 ID 的映射、重复/乱序完成、并发多附件、timeout/abort 与成功竞争以及晚到更新，并 SHALL 通过生成式性质测试验证下列性质

```pascal
// Property: Fix Checking - every false-failure input becomes a matching success
FOR ALL X WHERE isBugCondition(X) DO
  result ← processAttachment'(X)
  ASSERT result.http_response.status IN 200..299
  ASSERT isValidUploadSuccessPayload(result.http_response.body)
  ASSERT result.frontend_terminal_state = succeeded
  ASSERT result.frontend_attachment_id = X.backend_attachment_result.id
  ASSERT countTerminalTransitions(result, X.request_id) = 1
  ASSERT result.red_failure_indicator_visible = false
END FOR

// Property: Success payload serialization and parsing preserve the contract
FOR ALL valid upload success records R DO
  wire ← serializeUploadSuccess(R)
  parsed ← parseUploadSuccess'(wire)
  ASSERT parsed = normalizeCompatibleUploadFields(R)
  ASSERT parsed.error IN {null, absent}
END FOR

// Property: Correlation and terminal-state ordering are deterministic
FOR ALL generated event sequences S FOR distinct upload requests DO
  result ← reduceUploadEvents'(S)
  FOR EACH request R IN S DO
    ASSERT countTerminalTransitions(result, R.request_id) <= 1
    ASSERT noStateOwnedByOtherRequestChanged(result, R.request_id)
    IF acceptedSuccessPrecedesTimeoutOrAbort(S, R.request_id) THEN
      ASSERT terminalState(result, R.request_id) = succeeded
      ASSERT terminalAttachmentId(result, R.request_id) = successfulPayloadId(S, R.request_id)
    END IF
  END FOR
END FOR
```

### Unchanged Behavior (Regression Prevention)

3.1 WHEN 附件不满足 bug condition 且上传与处理成功 THEN the system SHALL CONTINUE TO 支持既有文件选择、拖放、粘贴、多附件上传、处理中反馈、成功显示、移除与消息提交行为

3.2 WHEN PNG/JPEG、PDF、DOC/DOCX、TXT、MD/Markdown 走既有处理路径 THEN the system SHALL CONTINUE TO 使用既有直接文本读取、PDF 文本与图片提取、OCR、嵌入图片处理和文档模型选择语义；对同一 PNG、Markdown 与 PDF 回归 fixture，修复前后 SHALL 保持已验证的提取文本、图片数量、附件元数据和模型调用语义不变

3.3 WHEN 成功附件随消息提交 THEN the system SHALL CONTINUE TO 通过既有附件引用和服务端保存结果完成文本/图片注入，并保持附件原文与图片数据不进入不必要的浏览器持久状态、会话快照、日志或错误消息

3.4 WHEN 上传真实地因未授权、配置无效、文件过大、不支持或扩展名/签名不匹配、存储失败、提取失败、独立确认的网络失败或成功前已生效的明确取消而失败 THEN the system SHALL CONTINUE TO 拒绝该上传并依据 lean-core Requirement 8.8 返回结构化、可恢复且不泄露敏感路径的错误，不得把真实失败标记为成功

3.5 WHEN 文件名、类型、大小、内容签名、上传 ID 或存储路径不符合既有安全约束 THEN the system SHALL CONTINUE TO 执行 allowlist、32 MiB 上限、文件名净化、规范 UUID、私有目录/文件权限、路径规范化与越界拒绝，不得为修复假失败而放宽安全边界

3.6 WHEN session 切换、清理、断线重连、页面恢复或多端同步发生 THEN the system SHALL CONTINUE TO 遵守既有 session/connection generation、事件去重、状态清理与服务端事实源语义，不得恢复已移除附件、把旧 session 或已失效请求的完成结果关联到当前附件或破坏重连后的上传/消息流程

3.7 WHEN ClientMsg、ServerMsg、EngineEvent 或上传 HTTP payload 保持当前兼容版本 THEN the system SHALL CONTINUE TO 满足 lean-core Requirement 2.5、8.6 的唯一权威映射和 schema 一致性约束，并保留 lean-core Requirement 8.7 所列附件及 session 管理协议

3.8 WHEN 输入不满足 `isBugCondition(X)` THEN the system SHALL CONTINUE TO 保持修复前相同的可观察接受/拒绝决定、成功数据、真实错误分类、附件引用、消息注入、安全决定和 session 副作用；比较时仅规范化生成式 UUID、trace ID、时间戳和等价 JSON 字段顺序，并 SHALL 通过 lean-core Requirement 11.6、11.7 要求的构建、静态检查及聚焦契约测试

```pascal
// Property: Preservation Checking
FOR ALL X WHERE NOT isBugCondition(X) DO
  before ← normalizeNondeterministicFields(observe(processAttachment(X)))
  after  ← normalizeNondeterministicFields(observe(processAttachment'(X)))
  ASSERT before = after
END FOR
```
