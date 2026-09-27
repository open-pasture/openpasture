import { useEffect, useRef, useState } from "react";
import { Menu } from "../ui";
import { layers, mountOverlay, type LayerItem, type MapHost, type OverlayHandle } from "./overlays";

// Bottom-left of the map: the layers that have data, each on or off. Nothing when none has.
export function LayersMenu({ host, herdId }: { host: MapHost; herdId?: string }) {
  const all = layers.use();
  const [on, setOn] = useState<string[]>([]);
  const mounted = useRef(new Map<string, OverlayHandle>());
  const avail = all.filter((l) => l.available());
  const availKey = avail.map((l) => l.id).join();

  // Mount what is on and available, drop the rest.
  useEffect(() => {
    const want = new Set(avail.filter((l) => on.includes(l.id)).map((l) => l.id));
    for (const [id, h] of mounted.current)
      if (!want.has(id)) {
        h.destroy();
        mounted.current.delete(id);
      }
    for (const l of avail)
      if (want.has(l.id) && !mounted.current.has(l.id)) mounted.current.set(l.id, mountOverlay(host, l.overlay));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [availKey, on, host]);

  useEffect(() => {
    mounted.current.forEach((h) => h.update?.());
  }, [herdId]);

  useEffect(() => () => {
    mounted.current.forEach((h) => h.destroy());
    mounted.current.clear();
  }, []);

  if (!avail.length) return null;
  const toggle = (l: LayerItem) => setOn((cur) => (cur.includes(l.id) ? cur.filter((x) => x !== l.id) : [...cur, l.id]));
  return (
    <div className="layersmenu">
      <Menu trigger={<span className="btn quiet sm">Layers</span>}
        items={avail.map((l) => ({
          label: <><i className={"dot" + (on.includes(l.id) ? " ok" : "")} />{l.label}</>,
          current: on.includes(l.id),
          onSelect: () => toggle(l),
        }))} />
    </div>
  );
}
