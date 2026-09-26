// Renders the app icon (icons/source.png, 1024px) from the pixel mark in
// openpasture-web/partials/mark.html: 5x5 grid, fg cells, one blaze cell at
// bottom-right, on a dark rounded square. Then: bunx @tauri-apps/cli icon.
import sharp from "sharp";

const rows = ["XXX..", "X.X..", "XXX..", "...XX", "...XB"];
const FG = "#F3F2EA", BLAZE = "#FF6A2B", BG = "#0B0C09";
const S = 1024, INSET = 100, BODY = S - 2 * INSET, R = 185; // macOS icon grid
const CELL = 68, GAP = 24, MARK = 5 * CELL + 4 * GAP, O = (S - MARK) / 2;

let cells = "";
rows.forEach((row, y) =>
  [...row].forEach((c, x) => {
    if (c === ".") return;
    cells += `<rect x="${O + x * (CELL + GAP)}" y="${O + y * (CELL + GAP)}" width="${CELL}" height="${CELL}" fill="${c === "B" ? BLAZE : FG}"/>`;
  }),
);
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${S}" height="${S}" viewBox="0 0 ${S} ${S}">
<rect x="${INSET}" y="${INSET}" width="${BODY}" height="${BODY}" rx="${R}" fill="${BG}"/>
<g shape-rendering="crispEdges">${cells}</g></svg>`;

await Bun.write(new URL("../src-tauri/icons/source.svg", import.meta.url), svg);
await sharp(Buffer.from(svg)).png().toFile(new URL("../src-tauri/icons/source.png", import.meta.url).pathname);
console.log("wrote src-tauri/icons/source.png");
