import fixtures from "./protocol-fixtures.json";
import type { DoneResult, ErrorMsg, EventMsg, MessagesLoadedMsg } from "./types";

function isRunMeta(value: Record<string, unknown>): boolean {
  return value.protocol_version === 1
    && typeof value.run_id === "string"
    && typeof value.session_id === "string"
    && typeof value.session_revision === "number"
    && typeof value.sequence === "number"
    && typeof value.timestamp_ms === "number";
}

function isEventMsg(value: unknown): value is EventMsg {
  if (!value || typeof value !== "object") return false;
  const message = value as Record<string, unknown>;
  return message.type === "event" && isRunMeta(message)
    && !!message.event && typeof message.event === "object";
}

function isSnapshotMsg(value: unknown): value is MessagesLoadedMsg {
  if (!value || typeof value !== "object") return false;
  const message = value as Record<string, unknown>;
  return message.type === "messages_loaded"
    && message.protocol_version === 1
    && typeof message.session_id === "string"
    && typeof message.revision === "number"
    && typeof message.timestamp_ms === "number"
    && Array.isArray(message.messages);
}

function isDoneMsg(value: unknown): value is DoneResult {
  if (!value || typeof value !== "object") return false;
  const message = value as Record<string, unknown>;
  return message.type === "done" && isRunMeta(message)
    && typeof message.text === "string"
    && typeof message.turns === "number"
    && !!message.usage && typeof message.usage === "object";
}

function isErrorMsg(value: unknown): value is ErrorMsg {
  if (!value || typeof value !== "object") return false;
  const message = value as Record<string, unknown>;
  return message.type === "error" && isRunMeta(message)
    && typeof message.message === "string";
}

if (!isEventMsg(fixtures.event)
  || !isSnapshotMsg(fixtures.snapshot)
  || !isDoneMsg(fixtures.done)
  || !isErrorMsg(fixtures.error)) {
  throw new Error("Rust/TypeScript WebSocket protocol fixture is inconsistent");
}

export const checkedProtocolFixtures = fixtures;
