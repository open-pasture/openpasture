import { useStore } from "../../store";
import { pageHash, pageKeys } from "../k-animals/herd";

// Herd panel: collars added but never heard from, each a link to its console. Goes away
// on the first report.
export function Waiting({ herdId }: { herdId: string }) {
  const collars = useStore((s) => s.collars);
  const animals = useStore((s) => s.animals);
  const waiting = collars.filter((c) => c.herd_id === herdId && !c.last_seen && !c.parked_at && !c.simulated);
  if (!waiting.length) return null;
  const key = pageKeys(animals);
  return (
    <ul className="fwait">
      {waiting.map((c) => {
        const animal = animals.find((a) => a.id === c.animal_id);
        return (
          <li key={c.id}>
            <a href={`#${pageHash(key(animal, c))}`}>{animal?.tag ?? c.name}</a>
            <span className="dim">waiting for its first report</span>
          </li>
        );
      })}
    </ul>
  );
}
