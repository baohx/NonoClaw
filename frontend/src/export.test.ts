import { combinedStructureError, createRasterExportArtifact, validateExportArtifact } from "./export.ts";

function check(value: boolean, message: string): void { if (!value) throw new Error(`export invariant failed: ${message}`); }

const pngPage = {
  bytes: Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a),
  width: 1588,
  height: 2246,
  mime: "image/png" as const,
};
const jpegPage = {
  bytes: Uint8Array.of(0xff, 0xd8, 0xff, 0xd9),
  width: 1588,
  height: 2246,
  mime: "image/jpeg" as const,
};

const docx = createRasterExportArtifact("docx", [pngPage, pngPage]);
validateExportArtifact(docx);
const docxRaw = new TextDecoder().decode(docx.bytes);
check(docx.pageCount === 2 && docx.filename.endsWith(".docx"), "DOCX must retain rendered page count and extension");
check(docx.bytes[0] === 0x50 && docx.bytes[1] === 0x4b, "DOCX must be an OOXML ZIP");
check(docxRaw.includes("word/media/page-1.png") && docxRaw.includes("word/media/page-2.png"), "DOCX must embed every rendered page");
check(docxRaw.includes("<w:pageBreakBefore/>") && docxRaw.includes("relationships/image"), "DOCX must paginate and relate rendered images");
check(docxRaw.includes("word/styles.xml") && docxRaw.includes("word/settings.xml"), "DOCX must include Word compatibility parts");
check(!docxRaw.includes('w:lineRule="exact"') && docxRaw.includes('w:pgMar w:top="720"'), "DOCX images must not be clipped by exact line height and must fit standard margins");

const pdf = createRasterExportArtifact("pdf", [jpegPage, jpegPage]);
validateExportArtifact(pdf);
const pdfText = new TextDecoder().decode(pdf.bytes);
check(pdf.pageCount === 2 && pdfText.startsWith("%PDF-") && pdfText.trimEnd().endsWith("%%EOF"), "PDF must have a valid multipage envelope");
check(pdfText.includes("/Count 2") && (pdfText.match(/\/Subtype \/Image/g) || []).length === 2, "PDF must embed every rendered page as an image");
check(pdfText.includes("/DCTDecode") && pdfText.includes("startxref"), "PDF rendered images and xref must be declared");

const combined = combinedStructureError({ docx: new Error("private body"), pdf: new Error("private body") });
check(combined?.message === "Export structure validation failed for: DOCX, PDF", "dual failures must produce one content-free combined error");
console.log("frontend rendered export checks passed");
