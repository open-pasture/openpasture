import { useCan } from "../../store/me";
import { Segmented } from "../../ui";
import { Build } from "./Build";
import { FirmwareTab } from "./Firmware";
import { NetworkTab } from "./Network";
import { Plans } from "./Plans";
import { Stations } from "./Stations";
import { Units } from "./Units";

type Tab = "units" | "build" | "plans" | "firmware" | "network" | "stations";
const TABS: { value: Tab; label: string }[] = [
  { value: "units", label: "Units" },
  { value: "build", label: "Build" },
  { value: "plans", label: "Test plans" },
  { value: "firmware", label: "Firmware" },
  { value: "network", label: "Network" },
  { value: "stations", label: "Stations" },
];

// #/fleet/<tab>/<rest>: building, testing and running collars, for owners and managers.
export function FleetView({ rest }: { rest: string }) {
  const [first, ...more] = rest.split("/");
  const owner = useCan("owner");
  const tabs = TABS.filter((t) => t.value !== "stations" || owner);
  const tab = (tabs.find((t) => t.value === first)?.value ?? "units") as Tab;
  const sub = more.map(decodeURIComponent).join("/");
  return (
    <div className="fleet">
      <div className="fbar">
        <Segmented label="Fleet" value={tab} options={tabs} onChange={(t) => (location.hash = `#/fleet/${t}`)} />
      </div>
      <div className="fbody">
        {tab === "units" && <Units serial={sub || undefined} />}
        {tab === "build" && <Build />}
        {tab === "plans" && <Plans />}
        {tab === "firmware" && <FirmwareTab />}
        {tab === "network" && <NetworkTab />}
        {tab === "stations" && owner && <Stations />}
      </div>
    </div>
  );
}
