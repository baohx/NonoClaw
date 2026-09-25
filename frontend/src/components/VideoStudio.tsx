import { useCallback, useEffect, useRef, useState } from "react";
import { getBrowserAccessToken } from "../security";

// ── Wire types (mirror serve_http/video_service.rs) ───────────────────────

interface VideoCapabilities {
  resolutions?: string[];
  durations?: [number, number];
  maxRefImages?: number;
  modes?: string[];
  draft?: boolean;
  pricing?: { unit?: string; with_video_input?: number; without_video_input?: number };
}

interface VideoModel {
  name: string;
  label?: string;
  default?: boolean;
  capabilities?: VideoCapabilities;
}

export interface VideoTaskView {
  id: string;
  remoteId?: string | null;
  model: string;
  mode: string;
  prompt: string;
  imageCount: number;
  duration: number;
  resolution: string;
  ratio: string;
  draft: boolean;
  status: string;
  error?: string | null;
  file?: string | null;
  createdAt: number;
  updatedAt: number;
}

// ── Helpers ────────────────────────────────────────────────────────────────

function api(path: string): string {
  const token = getBrowserAccessToken(window.location.search);
  return token ? `${path}?token=${encodeURIComponent(token)}` : path;
}

const MODE_LABELS: Record<string, string> = {
  t2v: "文生视频",
  i2v: "图生视频",
  reference: "多帧参考",
  first_last_frame: "首尾帧",
};

const STATUS_LABELS: Record<string, string> = {
  queued: "排队中",
  submitted: "已提交",
  running: "生成中",
  succeeded: "完成",
  failed: "失败",
  cancelled: "已删除",
};

function statusLabel(status: string): string {
  if (status.startsWith("queued+")) {
    const position = Number(status.slice(7));
    return `排队中 #${position + 1}`;
  }
  return STATUS_LABELS[status] ?? status;
}

/** Rough per-task cost estimate (per-1k-token console pricing). Resolution
 *  and ratio scale the token count; treated as an order-of-magnitude range. */
function estimateCost(model: VideoModel | null, seconds: number, resolution: string): string {
  const price = model?.capabilities?.pricing?.without_video_input ?? 0.023;
  const resolutionFactor = resolution === "1080p" ? 3.5 : resolution === "720p" ? 2 : 1;
  // ~700–1100 tokens per second at 720p 16:9 (empirical band from pricing).
  const tokensLow = Math.round(seconds * resolutionFactor * 700);
  const tokensHigh = Math.round(seconds * resolutionFactor * 1100);
  const low = ((tokensLow / 1000) * price).toFixed(2);
  const high = ((tokensHigh / 1000) * price).toFixed(2);
  return `¥${low}–¥${high}`;
}

// ── Component ──────────────────────────────────────────────────────────────

interface Props {
  onClose: () => void;
}

type Step = 1 | 2 | 3 | 4;

export default function VideoStudio({ onClose }: Props) {
  const [step, setStep] = useState<Step>(1);
  const [models, setModels] = useState<VideoModel[]>([]);
  const [modelsLoaded, setModelsLoaded] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const [mode, setMode] = useState<string>("t2v");
  const [images, setImages] = useState<{ name: string; url: string }[]>([]);
  const [modelName, setModelName] = useState<string>("");
  const [duration, setDuration] = useState(5);
  const [resolution, setResolution] = useState("720p");
  const [ratio, setRatio] = useState("16:9");
  const [draft, setDraft] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [tasks, setTasks] = useState<VideoTaskView[]>([]);
  const pollRef = useRef<number | null>(null);

  const model = models.find((m) => m.name === modelName) ?? null;
  const capabilities = model?.capabilities;

  // Model list load.
  useEffect(() => {
    fetch(api("/api/video/models"))
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data: { models: VideoModel[] }) => {
        setModels(data.models ?? []);
        setModelsLoaded(true);
        const preferred = data.models?.find((m) => m.default) ?? data.models?.[0];
        if (preferred) {
          setModelName(preferred.name);
          const cap = preferred.capabilities;
          if (cap?.resolutions?.length) setResolution(cap.resolutions.includes("720p") ? "720p" : cap.resolutions[Math.min(1, cap.resolutions.length - 1)]);
        }
      })
      .catch((e: Error) => setModelsError(e.message));
  }, []);

  // Task list poll: 5s while any in-flight, paused when tab hidden.
  const refreshTasks = useCallback(() => {
    if (document.hidden) return;
    fetch(api("/api/video/tasks"))
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data: { tasks: VideoTaskView[] }) => setTasks(data.tasks ?? []))
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshTasks();
    pollRef.current = window.setInterval(refreshTasks, 5000);
    const onVisibility = () => {
      if (!document.hidden) refreshTasks();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      if (pollRef.current) window.clearInterval(pollRef.current);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refreshTasks]);

  // Mode change resets incompatible images.
  const pickMode = (next: string) => {
    setMode(next);
    if (next === "t2v") setImages([]);
    if (next === "first_last_frame" && images.length > 2) setImages(images.slice(0, 2));
  };

  const maxImages =
    mode === "first_last_frame" ? 2 : (capabilities?.maxRefImages ?? (mode === "t2v" ? 0 : 9));

  const addImages = (files: FileList | null) => {
    if (!files) return;
    const next = [...images];
    for (const file of Array.from(files)) {
      if (next.length >= maxImages) break;
      next.push({ name: file.name, url: URL.createObjectURL(file) });
    }
    setImages(next);
  };

  // `图N` reference validation against the uploaded count.
  const promptRefWarning = (() => {
    const refs = [...prompt.matchAll(/图(?:片)?\s*(\d+)/g)].map((m) => Number(m[1]));
    const bad = refs.filter((n) => n > images.length);
    return bad.length ? `提示词引用了 图${bad.join("/图")}，但只上传了 ${images.length} 张` : null;
  })();

  const step1Valid =
    mode === "t2v" ||
    (mode === "first_last_frame" ? images.length === 2 : images.length >= 1);
  const [minDuration, maxDuration] = capabilities?.durations ?? [4, 15];

  const submit = async () => {
    setSubmitting(true);
    setSubmitError(null);
    try {
      const form = new FormData();
      form.append(
        "params",
        JSON.stringify({ model: modelName, mode, prompt, duration, resolution, ratio, draft }),
      );
      for (const image of images) {
        const blob = await fetch(image.url).then((r) => r.blob());
        form.append("image", blob, image.name);
      }
      const resp = await fetch(api("/api/video/tasks"), { method: "POST", body: form });
      if (!resp.ok) {
        const body = await resp.json().catch(() => ({}));
        throw new Error(body.error ?? `HTTP ${resp.status}`);
      }
      setPrompt("");
      setImages([]);
      setStep(1);
      refreshTasks();
    } catch (e) {
      setSubmitError((e as Error).message);
    } finally {
      setSubmitting(false);
    }
  };

  const deleteTask = async (id: string) => {
    await fetch(api(`/api/video/tasks/${id}`), { method: "DELETE" }).catch(() => {});
    refreshTasks();
  };

  const inFlight = tasks.some(
    (t) => !["succeeded", "failed", "cancelled"].includes(t.status),
  );

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog"
        style={{ maxWidth: 680, maxHeight: "88vh", overflowY: "auto" }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog__eyebrow mint">video studio</div>
        <div className="dialog__title">视频生成</div>

        {modelsError ? (
          <div style={{ padding: "16px 0", color: "var(--danger, #e5484d)" }}>
            无法加载模型配置：{modelsError}
            <div style={{ marginTop: 8, fontSize: 11, color: "var(--faint)" }}>
              在 settings.json 的 videoModels[] 中配置（apiKey 支持 $ENV 引用）
            </div>
          </div>
        ) : !modelsLoaded ? (
          <div style={{ padding: "16px 0", color: "var(--muted)" }}>加载模型中…</div>
        ) : (
          <div style={{ padding: "16px 0", color: "var(--danger, #e5484d)" }}>
            未配置任何视频模型。
            <div style={{ marginTop: 8, fontSize: 11, color: "var(--faint)" }}>
              在 settings.json 的 videoModels[] 中配置（apiKey 支持 $ENV 引用），重启 serve 后生效
            </div>
          </div>
        )}
        {modelsLoaded && models.length > 0 && (
          <>
            {/* Step indicator */}
            <div style={{ display: "flex", gap: 8, margin: "14px 0 16px", fontSize: 11 }}>
              {["1 素材", "2 规格", "3 提示词", "4 提交"].map((label, i) => (
                <div
                  key={label}
                  style={{
                    flex: 1,
                    textAlign: "center",
                    padding: "5px 0",
                    borderRadius: 6,
                    background:
                      step === i + 1 ? "var(--accent-dim, rgba(0,180,120,.15))" : "var(--bg-soft, #1c1c1e)",
                    color: step === i + 1 ? "var(--mint, #00b478)" : "var(--faint)",
                    fontWeight: step === i + 1 ? 600 : 400,
                  }}
                >
                  {label}
                </div>
              ))}
            </div>

            {step === 1 && (
              <div>
                <div style={{ display: "flex", gap: 8, marginBottom: 12 }}>
                  {(Object.keys(MODE_LABELS) as string[]).map((m) => (
                    <button
                      key={m}
                      className={`btn ${mode === m ? "btn--primary" : "btn--ghost"}`}
                      style={{ flex: 1, fontSize: 12 }}
                      onClick={() => pickMode(m)}
                    >
                      {MODE_LABELS[m]}
                    </button>
                  ))}
                </div>
                {mode !== "t2v" && (
                  <div>
                    <div style={{ fontSize: 11, color: "var(--muted)", marginBottom: 6 }}>
                      {mode === "first_last_frame"
                        ? "上传首帧与尾帧（顺序即首尾）"
                        : `上传参考图，可多选（上限 ${maxImages} 张）；提示词用 图1/图2 引用`}
                    </div>
                    <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                      {images.map((img, i) => (
                        <div
                          key={i}
                          style={{ position: "relative", width: 84, height: 84, borderRadius: 8, overflow: "hidden", border: "1px solid var(--border, #333)" }}
                        >
                          <img src={img.url} alt={img.name} style={{ width: "100%", height: "100%", objectFit: "cover" }} />
                          {mode === "first_last_frame" && (
                            <span style={{ position: "absolute", left: 4, top: 4, fontSize: 10, background: "rgba(0,0,0,.65)", color: "#fff", borderRadius: 4, padding: "1px 5px" }}>
                              {i === 0 ? "首帧" : "尾帧"}
                            </span>
                          )}
                          <button
                            style={{ position: "absolute", right: 3, top: 3, width: 18, height: 18, borderRadius: "50%", border: "none", background: "rgba(0,0,0,.65)", color: "#fff", cursor: "pointer", fontSize: 11, lineHeight: 1 }}
                            onClick={() => setImages(images.filter((_, j) => j !== i))}
                          >
                            ×
                          </button>
                        </div>
                      ))}
                      {images.length < maxImages && (
                        <label
                          style={{
                            width: 84, height: 84, borderRadius: 8, border: "1px dashed var(--border, #444)",
                            display: "flex", alignItems: "center", justifyContent: "center",
                            color: "var(--faint)", fontSize: 22, cursor: "pointer",
                          }}
                        >
                          +
                          <input
                            type="file"
                            accept="image/png,image/jpeg,image/webp"
                            multiple={mode !== "first_last_frame" || images.length === 0}
                            style={{ display: "none" }}
                            onChange={(e) => addImages(e.target.files)}
                          />
                        </label>
                      )}
                    </div>
                  </div>
                )}
              </div>
            )}

            {step === 2 && (
              <div style={{ display: "grid", gap: 12 }}>
                <label style={{ fontSize: 12 }}>
                  模型
                  <select value={modelName} onChange={(e) => setModelName(e.target.value)} style={{ width: "100%", marginTop: 4 }}>
                    {models.map((m) => (
                      <option key={m.name} value={m.name}>
                        {m.label ?? m.name}
                      </option>
                    ))}
                  </select>
                </label>
                <label style={{ fontSize: 12 }}>
                  时长：{Math.max(minDuration, Math.min(maxDuration, duration))}s（{minDuration}–{maxDuration}s）
                  <input
                    type="range"
                    min={minDuration}
                    max={maxDuration}
                    value={Math.max(minDuration, Math.min(maxDuration, duration))}
                    onChange={(e) => setDuration(Number(e.target.value))}
                    style={{ width: "100%", marginTop: 6 }}
                  />
                </label>
                <label style={{ fontSize: 12 }}>
                  分辨率
                  <select
                    value={capabilities?.resolutions?.includes(resolution) ? resolution : (capabilities?.resolutions?.[Math.min(1, (capabilities?.resolutions?.length ?? 2) - 1)] ?? "720p")}
                    onChange={(e) => setResolution(e.target.value)}
                    style={{ width: "100%", marginTop: 4 }}
                  >
                    {(capabilities?.resolutions ?? ["720p"]).map((r) => (
                      <option key={r} value={r}>{r}</option>
                    ))}
                  </select>
                </label>
                <label style={{ fontSize: 12 }}>
                  画面比例
                  <select value={ratio} onChange={(e) => setRatio(e.target.value)} style={{ width: "100%", marginTop: 4 }}>
                    {["16:9", "9:16", "1:1", "4:3", "3:4", "21:9"].map((r) => (
                      <option key={r} value={r}>{r}</option>
                    ))}
                  </select>
                </label>
                {capabilities?.draft && (
                  <label style={{ fontSize: 12, display: "flex", alignItems: "center", gap: 6 }}>
                    <input type="checkbox" checked={draft} onChange={(e) => setDraft(e.target.checked)} />
                    草稿模式（低价快速预览，无 1080p）
                  </label>
                )}
              </div>
            )}

            {step === 3 && (
              <div>
                <textarea
                  value={prompt}
                  onChange={(e) => setPrompt(e.target.value)}
                  placeholder="描述镜头：主体 → 动作 → 镜头语言（推/拉/摇/移）→ 景别 → 光线 → 风格。参考图用 图1/图2 引用（顺序与上传一致）。"
                  rows={7}
                  style={{ width: "100%", fontSize: 13, fontFamily: "var(--font-mono)", resize: "vertical" }}
                />
                <div style={{ display: "flex", justifyContent: "space-between", marginTop: 6, fontSize: 11 }}>
                  <span style={{ color: promptRefWarning ? "var(--danger, #e5484d)" : "var(--faint)" }}>
                    {promptRefWarning ?? `${prompt.length} 字`}
                  </span>
                  <span style={{ color: "var(--faint)" }}>并发 3 · RPM 180</span>
                </div>
              </div>
            )}

            {step === 4 && (
              <div style={{ display: "grid", gap: 10, fontSize: 12 }}>
                <div style={{ background: "var(--bg-soft, #1c1c1e)", borderRadius: 8, padding: "10px 12px" }}>
                  <div style={{ display: "grid", "gridTemplateColumns": "90px 1fr" as never, rowGap: 4 }}>
                    <span style={{ color: "var(--faint)" }}>模式</span><span>{MODE_LABELS[mode]}{images.length ? ` · ${images.length} 图` : ""}</span>
                    <span style={{ color: "var(--faint)" }}>模型</span><span>{model?.label ?? modelName}</span>
                    <span style={{ color: "var(--faint)" }}>规格</span><span>{duration}s · {resolution} · {ratio}{draft ? " · 草稿" : ""}</span>
                  </div>
                </div>
                <div style={{ background: "var(--accent-dim, rgba(0,180,120,.1))", borderRadius: 8, padding: "10px 12px" }}>
                  预估费用 <b>{estimateCost(model, duration, resolution)}</b>
                  <span style={{ color: "var(--faint)", marginLeft: 6, fontSize: 11 }}>（估算值，按实际 token 计费）</span>
                </div>
                {prompt.trim().length === 0 && (
                  <div style={{ color: "var(--danger, #e5484d)", fontSize: 11 }}>提示词为空</div>
                )}
                {submitError && (
                  <div style={{ color: "var(--danger, #e5484d)" }}>{submitError}</div>
                )}
              </div>
            )}

            {/* Wizard nav */}
            <div className="dialog__actions" style={{ marginTop: 14 }}>
              <button className="btn btn--ghost" onClick={onClose}>关闭</button>
              {step > 1 && (
                <button className="btn btn--ghost" onClick={() => setStep((step - 1) as Step)}>
                  上一步
                </button>
              )}
              {step < 4 && (
                <button
                  className="btn btn--primary"
                  disabled={step === 1 && !step1Valid}
                  onClick={() => setStep((step + 1) as Step)}
                >
                  下一步
                </button>
              )}
              {step === 4 && (
                <button
                  className="btn btn--primary"
                  disabled={submitting || prompt.trim().length === 0 || !!promptRefWarning}
                  onClick={submit}
                >
                  {submitting ? "提交中…" : `生成 · ${estimateCost(model, duration, resolution)}`}
                </button>
              )}
            </div>

            {/* Task list */}
            <div style={{ marginTop: 18, borderTop: "1px solid var(--border, #2a2a2c)", paddingTop: 12 }}>
              <div style={{ fontSize: 11, color: "var(--muted)", marginBottom: 8, display: "flex", justifyContent: "space-between" }}>
                <span>任务记录{inFlight ? " · 自动刷新中" : ""}</span>
                <button className="btn btn--ghost" style={{ padding: "2px 8px", fontSize: 11 }} onClick={refreshTasks}>
                  刷新
                </button>
              </div>
              {tasks.length === 0 && (
                <div style={{ fontSize: 11, color: "var(--faint)", padding: "8px 0" }}>暂无任务</div>
              )}
              {tasks.map((task) => (
                <div
                  key={task.id}
                  style={{ display: "flex", alignItems: "center", gap: 10, padding: "8px 0", borderTop: "1px solid var(--border, #222)", fontSize: 12 }}
                >
                  <span
                    style={{
                      width: 64, textAlign: "center", fontSize: 10, borderRadius: 4, padding: "3px 0",
                      background: task.status === "succeeded" ? "var(--accent-dim, rgba(0,180,120,.15))"
                        : task.status === "failed" ? "rgba(229,72,77,.15)"
                        : "var(--bg-soft, #1c1c1e)",
                      color: task.status === "succeeded" ? "var(--mint, #00b478)"
                        : task.status === "failed" ? "var(--danger, #e5484d)"
                        : "var(--muted)",
                    }}
                  >
                    {statusLabel(task.status)}
                  </span>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                      {MODE_LABELS[task.mode] ?? task.mode} · {task.duration}s · {task.resolution} ·{" "}
                      <span style={{ color: "var(--faint)" }}>{task.prompt.slice(0, 40)}</span>
                    </div>
                    {task.error && (
                      <div style={{ fontSize: 10, color: "var(--danger, #e5484d)", marginTop: 2 }}>{task.error}</div>
                    )}
                  </div>
                  {task.status === "succeeded" && task.file && (
                    <a
                      className="btn btn--ghost"
                      style={{ padding: "3px 10px", fontSize: 11 }}
                      href={api(`/api/video/tasks/${task.id}/file`)}
                      download
                    >
                      下载
                    </a>
                  )}
                  {task.status === "succeeded" && task.file && (
                    <video
                      src={api(`/api/video/tasks/${task.id}/file`)}
                      controls
                      preload="none"
                      style={{ width: 200, height: 112, borderRadius: 6, background: "#000" }}
                    />
                  )}
                  {!["running", "submitted"].includes(task.status) && task.status !== "queued" && (
                    <button
                      className="btn btn--ghost"
                      style={{ padding: "3px 10px", fontSize: 11 }}
                      onClick={() => deleteTask(task.id)}
                    >
                      删除
                    </button>
                  )}
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
