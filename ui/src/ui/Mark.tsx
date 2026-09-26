// The pixel OP mark (openpasture-web/partials/mark.html). "#" fg, "b" the blaze pixel.
const ROWS = ["###..", "#.#..", "###..", "...##", "...#b"];

export function Mark({ size = 18 }: { size?: number }) {
  return (
    <span className="mark" style={{ width: size, height: size }} aria-hidden="true">
      {ROWS.join("").split("").map((c, i) => (
        <i key={i} className={c === "." ? "o" : c === "b" ? "b" : undefined} />
      ))}
    </span>
  );
}
