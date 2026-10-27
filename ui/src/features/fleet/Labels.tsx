import { useEffect, useState } from "react";
import { fleet } from "../../api/fleet";

// #/print/fleet-labels/<serial>,<serial>…: one 50 × 25 mm label per unit, from the server's SVG
// (serial, class and board, claim code, and the card's QR code).
export function LabelsPage({ rest }: { rest: string }) {
  const serials = rest.split(/[/?]/)[0].split(",").map(decodeURIComponent).filter(Boolean);
  const [labels, setLabels] = useState<[string, string][]>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    Promise.all(serials.map(async (s) => [s, await fleet.label(s)] as [string, string])).then(setLabels, (e) => setErr((e as Error).message));
  }, [rest]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!serials.length) return <p className="dim">No units named.</p>;
  if (err) return <p className="err">{err}</p>;
  if (!labels) return null;
  return (
    <div className="flabels">
      {labels.map(([s, svg]) => <figure key={s} className="flabel" dangerouslySetInnerHTML={{ __html: svg }} />)}
    </div>
  );
}
