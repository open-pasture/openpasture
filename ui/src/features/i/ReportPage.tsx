// #/print/report/<id>?from=&to=[&herd_id=]: a report on white paper with its header block,
// tables, method notes and signature lines.

import { useEffect, useState } from "react";
import { reportsApi, type ReportDoc, type ReportSection } from "../../api/i";
import { useStore } from "../../store";
import { cellText, localToday, numeric, parsePrintRest, places } from "./format";

export default function ReportPage({ rest }: { rest: string }) {
  const tz = useStore((s) => s.state?.farm?.timezone);
  const [doc, setDoc] = useState<ReportDoc>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    const { id, q } = parsePrintRest(rest, localToday(tz));
    setDoc(undefined);
    setErr(undefined);
    reportsApi.get(id, q).then(setDoc, (e) => setErr((e as Error).message));
  }, [rest, tz]);
  // The browser's print header and a saved PDF take the page title.
  useEffect(() => {
    if (!doc) return;
    const was = document.title;
    document.title = `${doc.title} ${doc.from} – ${doc.to}`;
    return () => {
      document.title = was;
    };
  }, [doc]);

  if (err) return <p className="mono err">{err}</p>;
  if (!doc) return null;
  return (
    <article className="report">
      <h1>{doc.title}</h1>
      <dl className="rhead">
        {doc.header.map(([k, v]) => <div key={k}><dt>{k}</dt><dd>{v}</dd></div>)}
      </dl>
      {doc.sections.map((s, i) => <Table key={i} s={s} />)}
      {doc.notes.length > 0 && <ul className="rnotes">{doc.notes.map((n) => <li key={n}>{n}</li>)}</ul>}
      {doc.signatures.length > 0 && (
        <div className="sigs">
          {doc.signatures.map((l) => (
            <div className="sig" key={l}>
              <i /><i />
              <span>{l}</span><span>Date</span>
            </div>
          ))}
        </div>
      )}
    </article>
  );
}

// "Head-days" wraps at its space, never at its hyphen.
const nobreak = (label: string) => label.replace(/-/g, "\u2011");

function Table({ s }: { s: ReportSection }) {
  const cols = s.columns.map((c, j) => {
    const values = [...s.rows, ...(s.totals ? [s.totals] : [])].map((r) => r[j]);
    return { decimals: places(c, values), num: c.decimals !== undefined || numeric(s.rows.map((r) => r[j])) };
  });
  return (
    <section className="rsec">
      <h2>{s.title}</h2>
      <table className="tbl">
        <thead>
          <tr>{s.columns.map((c, j) => (
            <th key={c.key} className={cols[j].num ? "num" : undefined}>{nobreak(c.label)}{c.unit && <span className="unit"> {c.unit}</span>}</th>
          ))}</tr>
        </thead>
        <tbody>
          {s.rows.map((r, i) => (
            <tr key={i}>{r.map((v, j) => <td key={j} className={cols[j].num ? "num" : undefined}>{cellText(v, cols[j].decimals)}</td>)}</tr>
          ))}
        </tbody>
        {s.totals && (
          <tfoot>
            <tr>{s.totals.map((v, j) => <td key={j} className={cols[j].num ? "num" : undefined}>{cellText(v, cols[j].decimals)}</td>)}</tr>
          </tfoot>
        )}
      </table>
    </section>
  );
}
