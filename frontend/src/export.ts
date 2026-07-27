import html2canvas from "html2canvas";

const DOCX_MIME = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PDF_MIME = "application/pdf";
const PNG_MIME = "image/png";
const JPEG_MIME = "image/jpeg";

const A4_CSS_WIDTH = 794;
const A4_CSS_HEIGHT = 1123;
const PAGE_MARGIN = 48;
const PAGE_CONTENT_HEIGHT = A4_CSS_HEIGHT - PAGE_MARGIN * 2;
const RENDER_SCALE = 2;

export type ExportFormat = "docx" | "pdf";
export interface ExportArtifact {
  format: ExportFormat;
  bytes: Uint8Array;
  mime: string;
  filename: string;
  pageCount: number;
}
export interface RasterExportPage {
  bytes: Uint8Array;
  width: number;
  height: number;
  mime: typeof PNG_MIME | typeof JPEG_MIME;
}

function utf8(value: string): Uint8Array { return new TextEncoder().encode(value); }
function xml(value: string): string {
  return value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
function le16(value: number): Uint8Array { return Uint8Array.of(value & 255, value >>> 8 & 255); }
function le32(value: number): Uint8Array { return Uint8Array.of(value & 255, value >>> 8 & 255, value >>> 16 & 255, value >>> 24 & 255); }
function concat(parts: Uint8Array[]): Uint8Array {
  const output = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let offset = 0;
  for (const part of parts) { output.set(part, offset); offset += part.length; }
  return output;
}
function crc32(data: Uint8Array): number {
  let crc = 0xffffffff;
  for (const byte of data) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/** Minimal standards-compliant ZIP writer using stored entries, including binary media. */
function zip(entries: Array<[string, string | Uint8Array]>): Uint8Array {
  const local: Uint8Array[] = [];
  const central: Uint8Array[] = [];
  let offset = 0;
  for (const [name, content] of entries) {
    const nameBytes = utf8(name);
    const data = typeof content === "string" ? utf8(content) : content;
    const crc = crc32(data);
    const localHeader = concat([
      le32(0x04034b50), le16(20), le16(0x0800), le16(0), le16(0), le16(0), le32(crc),
      le32(data.length), le32(data.length), le16(nameBytes.length), le16(0), nameBytes,
    ]);
    local.push(localHeader, data);
    central.push(concat([
      le32(0x02014b50), le16(20), le16(20), le16(0x0800), le16(0), le16(0), le16(0),
      le32(crc), le32(data.length), le32(data.length), le16(nameBytes.length), le16(0), le16(0),
      le16(0), le16(0), le32(0), le32(offset), nameBytes,
    ]));
    offset += localHeader.length + data.length;
  }
  const centralBytes = concat(central);
  return concat([...local, centralBytes, le32(0x06054b50), le16(0), le16(0), le16(entries.length),
    le16(entries.length), le32(centralBytes.length), le32(offset), le16(0)]);
}

function docxDrawing(pageIndex: number): string {
  const relationshipId = `rId${pageIndex + 1}`;
  const pageBreak = pageIndex ? "<w:pageBreakBefore/>" : "";
  return `<w:p><w:pPr>${pageBreak}<w:spacing w:before="0" w:after="0"/><w:jc w:val="center"/></w:pPr><w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="6543000" cy="9254000"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:docPr id="${pageIndex + 1}" name="Rendered page ${pageIndex + 1}" descr="Rendered NonoClaw response page ${pageIndex + 1}"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="${pageIndex + 1}" name="page-${pageIndex + 1}.png"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="${relationshipId}" cstate="print"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="6543000" cy="9254000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>`;
}

function makeDocx(pages: RasterExportPage[]): Uint8Array {
  if (!pages.length || pages.some((page) => page.mime !== PNG_MIME)) throw new Error("DOCX export requires rendered PNG pages");
  const imageRelationships = pages.map((_, index) => `<Relationship Id="rId${index + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/page-${index + 1}.png"/>`).join("");
  const relationships = `${imageRelationships}<Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rIdSettings" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/>`;
  const document = pages.map((_, index) => docxDrawing(index)).join("");
  const entries: Array<[string, string | Uint8Array]> = [
    ["[Content_Types].xml", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/></Types>`],
    ["_rels/.rels", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>`],
    ["word/_rels/document.xml.rels", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">${relationships}</Relationships>`],
    ["word/styles.xml", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:eastAsia="Microsoft YaHei"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="0"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style></w:styles>`],
    ["word/settings.xml", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat><w:defaultTabStop w:val="720"/></w:settings>`],
    ["word/document.xml", `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><w:body>${document}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="720" w:right="720" w:bottom="720" w:left="720" w:header="0" w:footer="0" w:gutter="0"/></w:sectPr></w:body></w:document>`],
  ];
  pages.forEach((page, index) => entries.push([`word/media/page-${index + 1}.png`, page.bytes]));
  return zip(entries);
}

function pdfObject(id: number, body: string | Uint8Array): Uint8Array {
  const content = typeof body === "string" ? utf8(body) : body;
  return concat([utf8(`${id} 0 obj\n`), content, utf8("\nendobj\n")]);
}

function makePdf(pages: RasterExportPage[]): Uint8Array {
  if (!pages.length || pages.some((page) => page.mime !== JPEG_MIME)) throw new Error("PDF export requires rendered JPEG pages");
  const pageWidth = 595.28;
  const pageHeight = 841.89;
  const objects = new Map<number, Uint8Array>();
  const pageIds = pages.map((_, index) => 3 + index * 3);
  objects.set(1, pdfObject(1, "<< /Type /Catalog /Pages 2 0 R >>"));
  objects.set(2, pdfObject(2, `<< /Type /Pages /Kids [${pageIds.map((id) => `${id} 0 R`).join(" ")}] /Count ${pages.length} >>`));

  pages.forEach((page, index) => {
    const pageId = pageIds[index];
    const imageId = pageId + 1;
    const contentId = pageId + 2;
    const drawing = `q\n${pageWidth} 0 0 ${pageHeight} 0 0 cm\n/Im1 Do\nQ`;
    objects.set(pageId, pdfObject(pageId, `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ${pageWidth} ${pageHeight}] /Resources << /XObject << /Im1 ${imageId} 0 R >> >> /Contents ${contentId} 0 R >>`));
    objects.set(imageId, pdfObject(imageId, concat([
      utf8(`<< /Type /XObject /Subtype /Image /Width ${page.width} /Height ${page.height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length ${page.bytes.length} >>\nstream\n`),
      page.bytes,
      utf8("\nendstream"),
    ])));
    objects.set(contentId, pdfObject(contentId, `<< /Length ${utf8(drawing).length} >>\nstream\n${drawing}\nendstream`));
  });

  const maxId = 2 + pages.length * 3;
  const header = concat([utf8("%PDF-1.4\n%"), Uint8Array.of(0xe2, 0xe3, 0xcf, 0xd3), utf8("\n")]);
  const body: Uint8Array[] = [header];
  const offsets = new Array<number>(maxId + 1).fill(0);
  let offset = header.length;
  for (let id = 1; id <= maxId; id += 1) {
    const object = objects.get(id);
    if (!object) throw new Error(`PDF object ${id} is missing`);
    offsets[id] = offset;
    body.push(object);
    offset += object.length;
  }
  const xrefOffset = offset;
  const xref = `xref\n0 ${maxId + 1}\n0000000000 65535 f \n${offsets.slice(1).map((value) => `${String(value).padStart(10, "0")} 00000 n \n`).join("")}trailer\n<< /Size ${maxId + 1} /Root 1 0 R >>\nstartxref\n${xrefOffset}\n%%EOF\n`;
  return concat([...body, utf8(xref)]);
}

function safeFilename(stem: string): string {
  return stem.replace(/[^a-zA-Z0-9._-]+/g, "-").replace(/^[-.]+|[-.]+$/g, "") || "nonoclaw-export";
}

export function createRasterExportArtifact(format: ExportFormat, pages: RasterExportPage[], stem = "nonoclaw-export"): ExportArtifact {
  const filenameStem = safeFilename(stem);
  const artifact: ExportArtifact = format === "docx"
    ? { format, bytes: makeDocx(pages), mime: DOCX_MIME, filename: `${filenameStem}.docx`, pageCount: pages.length }
    : { format, bytes: makePdf(pages), mime: PDF_MIME, filename: `${filenameStem}.pdf`, pageCount: pages.length };
  validateExportArtifact(artifact);
  return artifact;
}

function pageSlices(totalHeight: number, boundaries: number[]): Array<{ start: number; end: number }> {
  const slices: Array<{ start: number; end: number }> = [];
  let start = 0;
  while (start < totalHeight - 1) {
    const target = Math.min(totalHeight, start + PAGE_CONTENT_HEIGHT);
    let end = target;
    if (target < totalHeight) {
      const candidates = boundaries.filter((boundary) => boundary > start + PAGE_CONTENT_HEIGHT * 0.45 && boundary <= target - 4);
      if (candidates.length) end = candidates[candidates.length - 1];
    }
    if (end <= start + 1) end = target;
    slices.push({ start, end });
    start = end;
  }
  return slices.length ? slices : [{ start: 0, end: 1 }];
}

async function canvasBytes(canvas: HTMLCanvasElement, mime: typeof PNG_MIME | typeof JPEG_MIME): Promise<Uint8Array> {
  const blob = await new Promise<Blob>((resolve, reject) => {
    canvas.toBlob((value) => value ? resolve(value) : reject(new Error("Browser could not encode the rendered export page")), mime, mime === JPEG_MIME ? 0.96 : undefined);
  });
  return new Uint8Array(await blob.arrayBuffer());
}

function replaceRenderedCanvases(source: HTMLElement, clone: HTMLElement): void {
  const sourceCanvases = Array.from(source.querySelectorAll("canvas"));
  const cloneCanvases = Array.from(clone.querySelectorAll("canvas"));
  sourceCanvases.forEach((canvas, index) => {
    const target = cloneCanvases[index];
    if (!target) return;
    try {
      const image = document.createElement("img");
      image.src = canvas.toDataURL(PNG_MIME);
      image.alt = canvas.getAttribute("aria-label") || "Rendered chart";
      image.className = target.className;
      image.style.cssText = target.style.cssText;
      image.style.width = `${canvas.getBoundingClientRect().width || canvas.width}px`;
      image.style.height = `${canvas.getBoundingClientRect().height || canvas.height}px`;
      image.style.display = "block";
      target.replaceWith(image);
    } catch {
      const context = target.getContext("2d");
      target.width = canvas.width;
      target.height = canvas.height;
      try { context?.drawImage(canvas, 0, 0); } catch {}
    }
  });
}

async function waitForRenderedDiagrams(source: HTMLElement): Promise<void> {
  const diagrams = Array.from(source.querySelectorAll<HTMLElement>(".mermaid-container"));
  if (!diagrams.length) return;
  const deadline = performance.now() + 3000;
  while (performance.now() < deadline) {
    if (diagrams.every((diagram) => diagram.querySelector("svg, .mermaid-raw"))) return;
    await new Promise<void>((resolve) => window.setTimeout(resolve, 50));
  }
}

function canvasHasVisibleInk(canvas: HTMLCanvasElement): boolean {
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (!context) return false;
  const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
  const step = Math.max(4, Math.floor(Math.sqrt((canvas.width * canvas.height) / 12000)) * 4);
  for (let index = 0; index < pixels.length; index += step) {
    if (pixels[index + 3] > 12 && (pixels[index] < 248 || pixels[index + 1] < 248 || pixels[index + 2] < 248)) return true;
  }
  return false;
}

async function replaceRenderedDiagrams(source: HTMLElement, clone: HTMLElement): Promise<void> {
  const selector = ".mermaid-container, .svg-container";
  const sourceDiagrams = Array.from(source.querySelectorAll<HTMLElement>(selector));
  const cloneDiagrams = Array.from(clone.querySelectorAll<HTMLElement>(selector));
  for (let index = 0; index < sourceDiagrams.length; index += 1) {
    const diagram = sourceDiagrams[index];
    const target = cloneDiagrams[index];
    if (!target || !diagram.querySelector("svg")) continue;
    const rect = diagram.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) continue;
    try {
      const canvas = await html2canvas(diagram, {
        backgroundColor: null,
        scale: RENDER_SCALE,
        useCORS: true,
        logging: false,
        foreignObjectRendering: true,
        width: Math.ceil(rect.width),
        height: Math.ceil(rect.height),
        windowWidth: Math.max(document.documentElement.clientWidth, Math.ceil(rect.right)),
        windowHeight: Math.max(document.documentElement.clientHeight, Math.ceil(rect.bottom)),
        scrollX: window.scrollX,
        scrollY: window.scrollY,
      });
      if (!canvasHasVisibleInk(canvas)) throw new Error("Diagram rasterization produced an empty image");
      const computed = getComputedStyle(diagram);
      const image = document.createElement("img");
      image.src = canvas.toDataURL(PNG_MIME);
      image.alt = diagram.classList.contains("mermaid-container") ? "Rendered Mermaid diagram" : "Rendered SVG diagram";
      image.dataset.exportRasterizedDiagram = "true";
      image.style.display = "block";
      image.style.width = `${rect.width}px`;
      image.style.height = `${rect.height}px`;
      image.style.maxWidth = "100%";
      image.style.objectFit = "contain";
      image.style.margin = `${computed.marginTop} ${computed.marginRight} ${computed.marginBottom} ${computed.marginLeft}`;
      target.replaceWith(image);
    } catch (reason) {
      console.warn("NonoClaw export could not pre-rasterize a diagram; falling back to page renderer", reason);
    }
  }
}

async function waitForImages(root: HTMLElement): Promise<void> {
  await Promise.all(Array.from(root.querySelectorAll("img")).map(async (image) => {
    if (image.complete) return;
    try { await image.decode(); } catch {}
  }));
}

async function captureRenderedPages(source: HTMLElement, format: ExportFormat): Promise<RasterExportPage[]> {
  if (!source.isConnected) throw new Error("The rendered response is no longer available for export");
  await document.fonts?.ready;
  await waitForRenderedDiagrams(source);

  const host = document.createElement("div");
  host.className = "rich-export-root";
  Object.assign(host.style, {
    position: "fixed",
    left: "-100000px",
    top: "0",
    width: `${A4_CSS_WIDTH}px`,
    height: `${A4_CSS_HEIGHT}px`,
    zIndex: "-1",
    pointerEvents: "none",
  });
  host.style.setProperty("--bg", "#ffffff");
  host.style.setProperty("--glass", "#ffffff");
  host.style.setProperty("--glass-2", "#ffffff");
  host.style.setProperty("--text", "#1d1d1f");
  host.style.setProperty("--muted", "#55555a");
  host.style.setProperty("--faint", "#77777d");
  host.style.setProperty("--border", "rgba(0,0,0,.14)");
  host.style.setProperty("--accent", "#0066cc");
  host.style.setProperty("--accent-soft", "rgba(0,102,204,.10)");
  host.style.setProperty("--accent-softer", "rgba(0,102,204,.06)");

  const style = document.createElement("style");
  style.textContent = `.rich-export-root,.rich-export-root *{animation:none!important;transition:none!important;caret-color:transparent!important}.rich-export-root{color:#1d1d1f;background:#fff;font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei","Noto Sans CJK SC",sans-serif}.rich-export-root .markdown-body{width:100%;max-width:none;color:#1d1d1f}.rich-export-root pre,.rich-export-root code{white-space:pre-wrap;overflow-wrap:anywhere}.rich-export-root table{max-width:100%}.rich-export-root button,.rich-export-root .echarts-block figcaption,.rich-export-root .echarts-block details{display:none!important}`;

  const viewport = document.createElement("div");
  Object.assign(viewport.style, {
    position: "relative",
    width: `${A4_CSS_WIDTH}px`,
    height: `${A4_CSS_HEIGHT}px`,
    overflow: "hidden",
    background: "#ffffff",
  });
  const clip = document.createElement("div");
  Object.assign(clip.style, {
    position: "absolute",
    left: `${PAGE_MARGIN}px`,
    top: `${PAGE_MARGIN}px`,
    width: `${A4_CSS_WIDTH - PAGE_MARGIN * 2}px`,
    height: `${PAGE_CONTENT_HEIGHT}px`,
    overflow: "hidden",
  });
  const content = source.cloneNode(true) as HTMLElement;
  Object.assign(content.style, {
    position: "absolute",
    left: "0",
    top: "0",
    width: "100%",
    maxWidth: "none",
    margin: "0",
  });
  replaceRenderedCanvases(source, content);
  await replaceRenderedDiagrams(source, content);
  content.querySelectorAll("button, .echarts-block figcaption, .echarts-block details").forEach((element) => element.remove());
  clip.appendChild(content);
  viewport.appendChild(clip);
  host.append(style, viewport);
  document.body.appendChild(host);

  try {
    await waitForImages(content);
    await new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
    const contentRect = content.getBoundingClientRect();
    const totalHeight = Math.max(1, Math.ceil(content.scrollHeight), Math.ceil(contentRect.height));
    const boundaries = Array.from(content.children).map((child) => {
      const rect = child.getBoundingClientRect();
      return Math.ceil(rect.bottom - contentRect.top);
    }).filter((value) => value > 0 && value < totalHeight);
    const slices = pageSlices(totalHeight, boundaries);
    const mime = format === "docx" ? PNG_MIME : JPEG_MIME;
    const pages: RasterExportPage[] = [];

    for (const slice of slices) {
      clip.style.height = `${Math.min(PAGE_CONTENT_HEIGHT, Math.max(1, slice.end - slice.start))}px`;
      content.style.transform = `translateY(-${slice.start}px)`;
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      const canvas = await html2canvas(viewport, {
        backgroundColor: "#ffffff",
        scale: RENDER_SCALE,
        useCORS: true,
        logging: false,
        width: A4_CSS_WIDTH,
        height: A4_CSS_HEIGHT,
        windowWidth: A4_CSS_WIDTH,
        windowHeight: A4_CSS_HEIGHT,
        scrollX: 0,
        scrollY: 0,
      });
      pages.push({ bytes: await canvasBytes(canvas, mime), width: canvas.width, height: canvas.height, mime });
    }
    return pages;
  } finally {
    host.remove();
  }
}

/** Export the browser-rendered response, including KaTeX, Mermaid, SVG and Canvas charts. */
export async function createRenderedExportArtifact(format: ExportFormat, renderedElement: HTMLElement, stem = "nonoclaw-export"): Promise<ExportArtifact> {
  const pages = await captureRenderedPages(renderedElement, format);
  return createRasterExportArtifact(format, pages, stem);
}

export function validateExportArtifact(artifact: ExportArtifact): void {
  const { bytes, format, mime, filename, pageCount } = artifact;
  if (pageCount < 1) throw new Error("Export contains no rendered pages");
  if (format === "docx") {
    if (mime !== DOCX_MIME || !filename.endsWith(".docx") || bytes[0] !== 0x50 || bytes[1] !== 0x4b) throw new Error("DOCX format metadata or ZIP signature mismatch");
    const raw = new TextDecoder().decode(bytes);
    for (const part of ["[Content_Types].xml", "_rels/.rels", "word/document.xml", "word/_rels/document.xml.rels", "word/styles.xml", "word/settings.xml", "word/media/page-1.png"]) {
      if (!raw.includes(part)) throw new Error(`DOCX structure missing ${part}`);
    }
  } else {
    const raw = new TextDecoder().decode(bytes);
    if (mime !== PDF_MIME || !filename.endsWith(".pdf") || !raw.startsWith("%PDF-") || !raw.trimEnd().endsWith("%%EOF") || !raw.includes("/Subtype /Image") || !raw.includes("startxref")) throw new Error("PDF structure or rendered page mismatch");
  }
}

/** Return one combined, content-free error when several requested structures fail. */
export function combinedStructureError(errors: Partial<Record<ExportFormat, unknown>>): Error | null {
  const formats = (["docx", "pdf"] as ExportFormat[]).filter((format) => errors[format]);
  if (!formats.length) return null;
  return new Error(`Export structure validation failed for: ${formats.map((format) => format.toUpperCase()).join(", ")}`);
}
