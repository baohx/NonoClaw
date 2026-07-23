import {
  clearMobileAccessToken,
  getMobileAccessToken,
  sanitizeBrowserText,
  sanitizeBrowserValue,
  sanitizeMediaAttachment,
  sanitizeProjectInfo,
  setMobileAccessToken,
} from "./security.ts";

function check(condition: boolean, message: string): void {
  if (!condition) throw new Error(`security invariant failed: ${message}`);
}

const filtered = sanitizeBrowserValue({
  apiKey: "sk-proj-browser-secret",
  nested: { authorization: "Bearer browser-secret", status: 503 },
  path: "src/main.ts",
});
const filteredText = JSON.stringify(filtered);
check(!filteredText.includes("sk-proj"), "API keys must be removed");
check(!filteredText.includes("Bearer"), "authorization values must be removed");
check(filteredText.includes("src/main.ts"), "safe technical metadata must remain");
check(sanitizeBrowserText("provider failed with Bearer token") === "[REDACTED]", "unsafe text must be redacted");

const attachment = sanitizeMediaAttachment({
  id: "upload-fixture",
  filename: "fixture.txt",
  extracted_text: "private attachment body sk-proj-attachment",
  image_count: 1,
  images: [{ media_type: "image/png", data: "private-image-data" }],
  uploading: false,
});
check(attachment.extracted_text === "", "attachment text must not enter the store");
check(attachment.images === undefined, "attachment images must not enter the store");
check(!JSON.stringify(attachment).includes("private"), "attachment content must be absent");

const tool = sanitizeBrowserValue({
  path: "src/main.ts",
  api_key: "sk-proj-tool-secret",
  headers: { authorization: "Bearer tool-secret" },
});
check(!JSON.stringify(tool).includes("sk-proj"), "tool inputs must not retain API keys");
check(!JSON.stringify(tool).includes("Bearer"), "tool inputs must not retain authorization");
check(JSON.stringify(tool).includes("src/main.ts"), "safe tool metadata must remain");

const projectInfo = sanitizeProjectInfo({
  cwd: "/fixture",
  model: "fixture",
  tools: [{ name: "Fixture", prompt_preview: "raw tool prompt sk-proj-prompt" }],
  skills: [{ name: "fixture", body: "raw skill body Bearer skill-secret" }],
  apiKey: "sk-proj-project-secret",
});
const storedProject = JSON.stringify(projectInfo);
check(!storedProject.includes("sk-proj"), "ProjectInfo must not retain API keys or prompts");
check(!storedProject.includes("Bearer"), "ProjectInfo must not retain credentials");
check(storedProject.includes("[tool prompt hidden]"), "tool metadata placeholder must remain");
check(storedProject.includes("[skill content kept server-side]"), "skill metadata placeholder must remain");

check(setMobileAccessToken("0123456789abcdef0123456789abcdef"), "valid mobile token must be available to QR components");
check(getMobileAccessToken() === "0123456789abcdef0123456789abcdef", "mobile token vault must round-trip in memory");
check(!setMobileAccessToken("Bearer unsafe token"), "unsafe mobile token format must be rejected");
check(getMobileAccessToken() === "", "rejected credentials must clear the vault");
clearMobileAccessToken();
