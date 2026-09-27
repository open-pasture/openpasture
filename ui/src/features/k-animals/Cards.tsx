import { useEffect, useState } from "react";
import { kAnimals, type Card } from "../../api/k-animals";
import { kAnimalsSlice } from "../../store/k-animals";
import { sheets } from "./herd";

// #/print/cards/<id>: one card per collar, 3 across and 4 down, each with its QR code,
// the animal's tag and the collar's name. The keys live only in this tab, so the cards
// print from the link or new-key response that made them.
export function CardsPage({ rest }: { rest: string }) {
  const id = rest.split(/[/?]/)[0];
  const batch = kAnimalsSlice.use((s) => s.batches[id]);
  const [cards, setCards] = useState<Map<string, string>>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    if (!batch) return;
    kAnimals.cards(batch.map((c) => ({ collar_id: c.collar.id, key: c.key })))
      .then((cs: Card[]) => setCards(new Map(cs.map((c) => [c.collar_id, c.qr_svg]))), (e) => setErr((e as Error).message));
  }, [batch]);

  if (!batch) return <p className="dim">Nothing to print: keys are shown once.</p>;
  if (err) return <p className="err">{err}</p>;
  if (!cards) return null;
  return (
    <div className="cards">
      {sheets(batch).map((sheet, i) => (
        <section key={i} className="cardsheet">
          {sheet.map((c) => (
            <figure key={c.collar.id} className="card">
              <div className="qr" dangerouslySetInnerHTML={{ __html: cards.get(c.collar.id) ?? "" }} />
              <figcaption>
                <b>{c.tag ?? c.collar.name}</b>
                {c.tag && <span className="mono">{c.collar.name}</span>}
              </figcaption>
            </figure>
          ))}
        </section>
      ))}
    </div>
  );
}
