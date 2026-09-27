// One line of controls matching a file's columns to the fields we need: "tag [Visual ID]
// time [Date] …". Required fields have no "none" choice.
export interface MappingField { key: string; label: string; required?: boolean }

export function MappingRow({ fields, columns, value, onChange }: {
  fields: MappingField[];
  columns: string[];
  value: Record<string, string | undefined>;
  onChange: (next: Record<string, string | undefined>) => void;
}) {
  return (
    <div className="maprow">
      {fields.map((f) => (
        <label key={f.key}>
          <span className="mono dim">{f.label}</span>
          <select className="input sm mono" value={value[f.key] ?? ""} aria-label={f.label}
            onChange={(e) => onChange({ ...value, [f.key]: e.target.value || undefined })}>
            {(!f.required || !value[f.key]) && <option value="">–</option>}
            {columns.map((c) => <option key={c} value={c}>{c}</option>)}
          </select>
        </label>
      ))}
    </div>
  );
}
