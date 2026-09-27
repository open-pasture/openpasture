// A small line of recent values, the last one marked. Gaps (null) break the line.
export function Spark({ values, width = 64, height = 16, domain, label }: {
  values: (number | null)[];
  width?: number;
  height?: number;
  domain?: [number, number]; // fixed y range, e.g. [0, 1] for battery
  label?: string;
}) {
  const nums = values.filter((v): v is number => v !== null && Number.isFinite(v));
  if (nums.length < 2) return null;
  let [lo, hi] = domain ?? [Math.min(...nums), Math.max(...nums)];
  if (hi - lo < 1e-9) (lo -= 1), (hi += 1);
  const x = (i: number) => (values.length === 1 ? width : (i / (values.length - 1)) * (width - 2) + 1);
  const y = (v: number) => height - 1.5 - ((v - lo) / (hi - lo)) * (height - 3);
  let d = "";
  let pen = false;
  values.forEach((v, i) => {
    if (v === null || !Number.isFinite(v)) return void (pen = false);
    d += `${pen ? "L" : "M"}${x(i).toFixed(1)} ${y(v).toFixed(1)}`;
    pen = true;
  });
  const li = values.length - 1 - [...values].reverse().findIndex((v) => v !== null && Number.isFinite(v));
  return (
    <svg className="spark" width={width} height={height} viewBox={`0 0 ${width} ${height}`} role={label ? "img" : undefined}
      aria-label={label} aria-hidden={label ? undefined : true}>
      <path d={d} />
      <rect x={x(li) - 1.5} y={y(values[li] as number) - 1.5} width={3} height={3} />
    </svg>
  );
}
