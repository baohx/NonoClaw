// Trajectory tree builder: folds the flat message stream + explicit subagent
// parent/child map into a multi-branch tree. Within the main branch the
// durable log is a linear chain (assistant turns carrying tool calls); each
// subagent run is its own branch attached at the parent tool-call node.

import type { ChatMessage, SubagentRun } from "../types";
import type {
  BranchInfo,
  ToolCallRecord,
  TrajectoryNode,
  TrajectoryTree,
  TurnRecord,
} from "./types";

export interface TreeInput {
  messages: ChatMessage[];
  subagentRunsById: Record<string, SubagentRun>;
  childIdsByParentToolId: Record<string, string[]>;
}

const MAIN_BRANCH: BranchInfo = {
  branchId: "main",
  branchType: "main",
  parentBranchId: null,
};

function toolUseIdOf(message: ChatMessage): string {
  return message.id.startsWith("tool-") ? message.id.slice("tool-".length) : message.id;
}

function buildSubagentBranch(
  runId: string,
  runsById: Record<string, SubagentRun>,
  childIdsByParentToolId: Record<string, string[]>,
  linearCalls: ToolCallRecord[],
  now: number | undefined,
): TrajectoryNode {
  const run = runsById[runId];
  const branch: BranchInfo = {
    branchId: `subagent:${runId}`,
    branchType: "subagent",
    parentBranchId: "main",
    label: run?.description || run?.profile || "Subagent",
  };
  const children: TrajectoryNode[] = [];
  for (const toolId of run?.toolOrder ?? []) {
    const tool = run.toolsById[toolId];
    if (!tool) continue;
    const nodeId = `subagent:${runId}:tool:${tool.id}`;
    linearCalls.push({
      nodeId,
      name: tool.name,
      input: tool.input,
      ok: tool.ok,
      result: tool.result,
      // Subagent tools carry no measured timestamp of their own — inherit the
      // parent tool-call's wall-clock (never synthesize `+ index` fake ms).
      timestamp: now,
      turn: undefined,
    });
    // Nested subagents (a subagent delegating further) attach at the tool
    // call that spawned them. NonoClaw caps subagent depth at 1, so this is
    // normally empty — kept correct for completeness.
    const toolChildren: TrajectoryNode[] = (childIdsByParentToolId[tool.id] ?? []).map((childId) =>
      buildSubagentBranch(childId, runsById, childIdsByParentToolId, linearCalls, now),
    );
    children.push({
      nodeId,
      kind: "tool",
      label: `${tool.name}${tool.ok === false ? " ✗" : ""}`,
      timestamp: now,
      toolName: tool.name,
      toolInput: tool.input,
      toolOk: tool.ok,
      toolResult: tool.result,
      children: toolChildren,
      branch,
    });
  }
  return {
    nodeId: `subagent:${runId}`,
    kind: "subagent",
    label: run?.description || run?.profile || `Subagent ${runId}`,
    timestamp: now,
    subagentStatus: run?.status,
    subagentProfile: run?.profile,
    children,
    branch,
  };
}

export function buildTrajectoryTree(input: TreeInput): TrajectoryTree {
  const { messages, subagentRunsById, childIdsByParentToolId } = input;
  const linearCalls: ToolCallRecord[] = [];
  const turns: TurnRecord[] = [];
  let userBaseline = "";

  const root: TrajectoryNode = {
    nodeId: "root",
    kind: "turn",
    label: "trajectory",
    children: [],
    branch: MAIN_BRANCH,
  };
  let currentTurn: TrajectoryNode | null = null;
  let turnNumber = 0;

  for (const message of messages) {
    if (message.role === "user") {
      if (!userBaseline && message.content.trim()) userBaseline = message.content;
    } else if (message.role === "assistant") {
      turnNumber += 1;
      const node: TrajectoryNode = {
        nodeId: `turn:${turnNumber}`,
        kind: "turn",
        label: `Turn ${turnNumber}`,
        timestamp: message.timestamp,
        children: [],
        branch: MAIN_BRANCH,
      };
      root.children.push(node);
      currentTurn = node;
      turns.push({
        turn: turnNumber,
        text: message.content,
        inputTokens: 0,
        outputTokens: 0,
        timestamp: message.timestamp,
      });
    } else if (message.role === "tool" && currentTurn) {
      const toolUseId = toolUseIdOf(message);
      const nodeId = `tool:${toolUseId}`;
      const node: TrajectoryNode = {
        nodeId,
        kind: "tool",
        label: `${message.toolName ?? "tool"}${message.toolOk === false ? " ✗" : ""}`,
        timestamp: message.timestamp,
        toolName: message.toolName,
        toolInput: message.toolInput,
        toolOk: message.toolOk,
        toolResult: message.content,
        children: [],
        branch: MAIN_BRANCH,
      };
      currentTurn.children.push(node);
      linearCalls.push({
        nodeId,
        name: message.toolName ?? "tool",
        input: message.toolInput,
        ok: message.toolOk,
        result: message.content,
        timestamp: message.timestamp,
        turn: turnNumber,
      });
      // Attach subagent branches spawned by this tool call.
      const childRunIds = childIdsByParentToolId[toolUseId] ?? [];
      for (const childId of childRunIds) {
        node.children.push(
          buildSubagentBranch(childId, subagentRunsById, childIdsByParentToolId, linearCalls, message.timestamp),
        );
      }
    }
  }

  return {
    root,
    nodesBuilt: linearCalls.length + turns.length + 1,
    subagentAttached: Object.keys(subagentRunsById).length,
    linearToolCalls: linearCalls,
    turns,
    userBaseline,
  };
}

export type { TurnRecord };
