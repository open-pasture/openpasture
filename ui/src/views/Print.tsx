import { useEffect } from "react";
import { guarded, printPages } from "../registry";
import { Button } from "../ui";

// #/print/<id>/<rest>: a registered page on white paper, without the app around it.
// The Print button is the only chrome and doesn't print.
export function Print({ rest }: { rest: string }) {
  const pages = printPages.use();
  const cut = rest.search(/[/?]/);
  const id = cut < 0 ? rest : rest.slice(0, cut);
  const pageRest = cut < 0 ? "" : rest.slice(rest[cut] === "/" ? cut + 1 : cut);
  const page = pages.find((p) => p.id === id);

  useEffect(() => {
    document.documentElement.classList.add("paper");
    return () => document.documentElement.classList.remove("paper");
  }, []);

  if (!page) return null;
  return (
    <div className="print">
      <div className="printbar">
        <Button small onClick={() => window.print()}>Print</Button>
      </div>
      {guarded(page.id, <page.Page rest={pageRest} />)}
    </div>
  );
}
