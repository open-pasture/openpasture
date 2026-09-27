import { expect, test } from "bun:test";
import { liveEvents } from "./x1";

test("a batch of several herds' batches is its events, in order", () => {
  const msg = {
    type: "batch",
    events: [
      { type: "positions", herd_id: "herd_1", items: [] },
      { type: "ack_batch", herd_id: "herd_2", items: [] },
      { type: "cue_batch", herd_id: "herd_3", items: [] },
    ],
  };
  expect(liveEvents(msg).map((e) => `${e.type} ${"herd_id" in e ? e.herd_id : ""}`)).toEqual(["positions herd_1", "ack_batch herd_2", "cue_batch herd_3"]);
});

test("any other message is one event", () => {
  const m = { type: "positions", herd_id: "herd_1", items: [] };
  expect(liveEvents(m)).toEqual([m as never]);
  expect(liveEvents({ type: "resync" })).toEqual([{ type: "resync" }]);
});

test("malformed messages and malformed batch members give nothing", () => {
  expect(liveEvents(null)).toEqual([]);
  expect(liveEvents("positions")).toEqual([]);
  expect(liveEvents({ items: [] })).toEqual([]);
  expect(liveEvents({ type: "batch" })).toEqual([]);
  expect(liveEvents({ type: "batch", events: [null, 3, { type: "resync" }] })).toEqual([{ type: "resync" }]);
});
