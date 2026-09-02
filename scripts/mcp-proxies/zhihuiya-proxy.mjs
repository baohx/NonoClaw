#!/usr/bin/env node
/**
 * Zhihuiya MCP Proxy
 * Bridges the remote Streamable HTTP MCP server to local stdio transport.
 */
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
  ListPromptsRequestSchema,
  GetPromptRequestSchema,
  ListResourceTemplatesRequestSchema,
  CompleteRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

const REMOTE_URL = "https://connect.zhihuiya.com/1458a4/mcp";
const API_KEY = process.env.ZHIHUIYA_API_KEY || "sk-W8jxCK57RfUAVwg4Kw2QcZdBR97cI7BXMQbyiWHLN0upHV2K";

async function main() {
  // 1. Connect to remote zhihuiya MCP server
  const remoteTransport = new StreamableHTTPClientTransport(
    new URL(REMOTE_URL),
    { requestInit: { headers: { Authorization: `Bearer ${API_KEY}` } } }
  );

  const remoteClient = new Client(
    { name: "zhihuiya-proxy", version: "1.0.0" }
  );

  console.error("[zhihuiya-proxy] Connecting to remote MCP...");
  await remoteClient.connect(remoteTransport);
  console.error("[zhihuiya-proxy] Connected to remote MCP");

  // 2. Get remote capabilities and info
  const remoteCaps = remoteClient.getServerCapabilities();
  const remoteInfo = remoteClient.getServerVersion();
  console.error(`[zhihuiya-proxy] Remote: ${remoteInfo?.name} v${remoteInfo?.version}`);
  console.error(`[zhihuiya-proxy] Caps: ${JSON.stringify(Object.keys(remoteCaps || {}))}`);

  // 3. Discover available tools/prompts/resources
  try {
    const tools = await remoteClient.listTools();
    console.error(`[zhihuiya-proxy] Tools: ${tools.tools.map(t => t.name).join(", ")}`);
  } catch (e) {
    console.error("[zhihuiya-proxy] No tools:", e.message);
  }

  // 4. Create local stdio server with matching capabilities
  const localCaps = {};
  if (remoteCaps?.tools) localCaps.tools = {};
  if (remoteCaps?.resources) localCaps.resources = {};
  if (remoteCaps?.prompts) localCaps.prompts = {};
  if (remoteCaps?.logging) localCaps.logging = {};

  const server = new Server(
    { name: remoteInfo?.name || "zhihuiya", version: remoteInfo?.version || "1.0.0" },
    { capabilities: localCaps }
  );

  // 5. Register proxy handlers for capabilities the remote supports
  if (remoteCaps?.tools) {
    server.setRequestHandler(ListToolsRequestSchema, async () => {
      return await remoteClient.listTools();
    });
    server.setRequestHandler(CallToolRequestSchema, async (req) => {
      return await remoteClient.callTool(req.params);
    });
  }

  if (remoteCaps?.resources) {
    server.setRequestHandler(ListResourcesRequestSchema, async (req) => {
      return await remoteClient.listResources(req?.params);
    });
    server.setRequestHandler(ReadResourceRequestSchema, async (req) => {
      return await remoteClient.readResource(req.params);
    });
    server.setRequestHandler(ListResourceTemplatesRequestSchema, async (req) => {
      return await remoteClient.listResourceTemplates(req?.params);
    });
  }

  if (remoteCaps?.prompts) {
    server.setRequestHandler(ListPromptsRequestSchema, async (req) => {
      return await remoteClient.listPrompts(req?.params);
    });
    server.setRequestHandler(GetPromptRequestSchema, async (req) => {
      return await remoteClient.getPrompt(req.params);
    });
  }

  if (remoteCaps?.completions) {
    server.setRequestHandler(CompleteRequestSchema, async (req) => {
      return await remoteClient.complete(req.params);
    });
  }

  // 6. Start stdio transport
  const stdioTransport = new StdioServerTransport();
  await server.connect(stdioTransport);
  console.error("[zhihuiya-proxy] Proxy ready on stdio");
}

main().catch((err) => {
  console.error("[zhihuiya-proxy] Fatal error:", err.message);
  process.exit(1);
});
