import { BreathController, type BreathVisualState } from "./breath.ts";
import type { EngineEvent } from "./types.ts";

function assert(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(`breath invariant failed: ${message}`);
}

function clock() {
  let now = 0;
  return {
    now: () => now,
    advance: (milliseconds: number) => { now += milliseconds; },
  };
}

function assertFrame(frame: BreathVisualState, context: string): void {
  for (const [name, value] of Object.entries(frame)) {
    if (typeof value === "number") assert(Number.isFinite(value), `${context}: ${name} must be finite`);
  }
  assert(frame.amplitude >= 0 && frame.amplitude <= 0.4, `${context}: amplitude must stay bounded`);
  assert(frame.frequency >= 0 && frame.frequency <= 1.2, `${context}: frequency must stay bounded`);
  assert(frame.turbulence >= 0 && frame.turbulence <= 1, `${context}: turbulence must stay bounded`);
  assert(frame.warmth >= 0 && frame.warmth <= 1, `${context}: warmth must stay bounded`);
  assert(frame.flare >= 0 && frame.flare <= 1, `${context}: flare must stay bounded`);
  assert(frame.velocity >= 0 && frame.velocity <= 1, `${context}: velocity must stay bounded`);
}

// **Validates: Requirements 10.1-10.3, 10.5, 10.8-10.9**
function stateMachineChecks(): void {
  const time = clock();
  const controller = new BreathController({ now: time.now, successHoldMs: 1000, errorHoldMs: 1200 });

  controller.consumeConnection("connecting");
  assert(controller.getSnapshot().phase === "connecting", "first connection must be connecting");
  controller.consumeConnection("connected");
  assert(controller.getSnapshot().phase === "idle", "connected without a run must be idle");

  controller.consume({ kind: "run_started" });
  assert(controller.getSnapshot().phase === "thinking", "run start must think");
  controller.consume({ kind: "text_delta", text: "hello" });
  assert(controller.getSnapshot().phase === "streaming", "text must stream");
  controller.consume({ kind: "tool_execution_started", tool_use_id: "tool-a" });
  assert(controller.getSnapshot().phase === "tool", "tool start must enter tool state");
  controller.consume({ kind: "permission_requested", tool_use_id: "tool-a", waiting_on: "permission" });
  assert(controller.getSnapshot().phase === "waiting-permission", "permission must have a distinct wait state");
  controller.consume({ kind: "permission_resolved", tool_use_id: "tool-a" });
  assert(controller.getSnapshot().phase === "tool", "permission resolution must resume the tool");
  controller.consume({ kind: "tool_execution_finished", tool_use_id: "tool-a", status: "succeeded" });
  assert(controller.getSnapshot().phase === "thinking", "tool completion must resume the run");

  controller.consumePrompt("question", true);
  assert(controller.getSnapshot().phase === "waiting-question", "questions must have a distinct wait state");
  controller.consumePrompt("question", false);
  assert(controller.getSnapshot().phase === "thinking", "answering must resume immediately");

  controller.consume({ kind: "compaction_started" });
  assert(controller.getSnapshot().phase === "compacting", "compaction must be represented");
  controller.consume({ kind: "compacted" });
  assert(controller.getSnapshot().phase === "thinking", "automatic compaction must resume the run");
  controller.consume({ kind: "subagent_started", description: "worker" });
  assert(controller.getSnapshot().phase === "subagent", "subagent start must be represented");
  controller.consume({ kind: "subagent_finished", description: "worker", status: "succeeded" });
  assert(controller.getSnapshot().phase === "thinking", "subagent completion must resume the run");

  controller.consume({ kind: "run_finished", status: "succeeded" });
  assert(controller.getSnapshot().phase === "success", "successful terminal must flare once");
  time.advance(1001);
  controller.sample();
  assert(controller.getSnapshot().phase === "idle", "success must decay to idle");

  controller.consume({ kind: "run_started" });
  controller.consume({ kind: "run_error", message: "fixture failure" });
  assert(controller.getSnapshot().phase === "error", "run error must be represented");
  time.advance(1201);
  controller.sample();
  assert(controller.getSnapshot().phase === "idle", "error must decay to idle");

  controller.consumeConnection("disconnected");
  assert(controller.getSnapshot().phase === "reconnecting", "later disconnects must reconnect non-blockingly");
  controller.consumeConnection("connected");
  assert(controller.getSnapshot().phase === "idle", "reconnection must settle");
}

// **Validates: Requirements 10.3-10.4**
function interpolationAndTokenChecks(): void {
  const time = clock();
  const controller = new BreathController({ now: time.now, transitionTauMs: 400 });
  controller.consumeConnection("connected");
  const idle = controller.sample();
  controller.consume({ kind: "run_started" });
  const sameInstant = controller.sample();
  assert(Math.abs(sameInstant.amplitude - idle.amplitude) < 0.000001, "state changes must not jump visual amplitude");
  time.advance(100);
  const transitioning = controller.sample();
  assert(transitioning.amplitude > idle.amplitude && transitioning.amplitude < 0.18,
    "interpolation must approach rather than jump to the target");

  let emissions = 0;
  const unsubscribe = controller.subscribe(() => { emissions += 1; });
  const beforeLongStream = emissions;
  for (let index = 0; index < 50_000; index += 1) {
    controller.consume({ kind: "text_delta", text: "token" });
    if (index % 500 === 0) {
      time.advance(16);
      assertFrame(controller.sample(), `long stream frame ${index}`);
    }
  }
  assert(emissions - beforeLongStream === 1, "token deltas must not trigger per-token subscribers or React renders");
  const saturated = controller.sample();
  assert(saturated.velocity === 1, "long streams must saturate rather than accumulate unbounded energy");
  time.advance(2000);
  const decayed = controller.sample();
  assert(decayed.velocity < saturated.velocity, "token energy must decay after a stream gap");
  unsubscribe();
  const afterUnsubscribe = emissions;
  controller.consume({ kind: "tool_execution_started", tool_use_id: "cleanup" });
  assert(emissions === afterUnsubscribe, "subscription cleanup must stop notifications");
}

// **Validates: Requirements 10.6-10.7**
function environmentChecks(): void {
  const time = clock();
  const controller = new BreathController({ now: time.now });
  controller.consumeConnection("connected");
  controller.consume({ kind: "run_started" });
  controller.consume({ kind: "text_delta", text: "visible motion" });

  controller.setVisibility(true);
  assert(controller.sample().paused, "hidden documents must pause animation");
  controller.setVisibility(false);
  assert(!controller.sample().paused, "visible documents must resume animation");

  controller.setReducedMotion(true);
  const reduced = controller.sample();
  assert(reduced.paused && reduced.amplitude === 0 && reduced.frequency === 0 && reduced.turbulence === 0,
    "reduced motion must remove nonessential movement");
  assert(controller.getSnapshot().label === "streaming", "reduced motion must preserve textual state");
  controller.setReducedMotion(false);
  time.advance(16);
  assert(!controller.sample().paused, "normal motion must be restorable");
}

// **Validates: Requirements 10.1-10.9**
function generatedSequenceChecks(): void {
  const time = clock();
  const controller = new BreathController({ now: time.now });
  controller.consumeConnection("connected");
  const events: EngineEvent[] = [
    { kind: "run_started" },
    { kind: "stream_state_changed", state: "thinking" },
    { kind: "text_delta", text: "abc" },
    { kind: "tool_queued", tool_use_id: "generated" },
    { kind: "permission_requested", tool_use_id: "generated", waiting_on: "permission" },
    { kind: "permission_resolved", tool_use_id: "generated" },
    { kind: "tool_execution_finished", tool_use_id: "generated", status: "failed" },
    { kind: "retry_scheduled", delay_ms: 25 },
    { kind: "model_request_started" },
    { kind: "subagent_started", description: "generated" },
    { kind: "subagent_finished", description: "generated", status: "succeeded" },
    { kind: "run_finished", status: "succeeded" },
  ];

  for (let cycle = 0; cycle < 500; cycle += 1) {
    const event = events[cycle % events.length];
    controller.consume(event);
    time.advance((cycle % 17) + 1);
    assertFrame(controller.sample(), `generated sequence ${cycle}`);
  }
}

stateMachineChecks();
interpolationAndTokenChecks();
environmentChecks();
generatedSequenceChecks();
console.log("deterministic breath controller checks passed");
