import { expect, test } from "bun:test";
import { Aborted, ApiError } from "../api/http";
import { createAction, type ActionState } from "./action";

test("busy while it runs, then the server's sentence in place, and Try again runs the same call", async () => {
  const states: ActionState[] = [];
  let fail = true;
  const calls: string[] = [];
  const a = createAction(
    async (herd: string) => {
      calls.push(herd);
      if (fail) throw new ApiError(409, "The move already stopped.");
      return "ok";
    },
    (s) => states.push(s),
  );
  expect(await a.run("h1")).toBe(false);
  expect(states).toEqual([{ busy: true }, { busy: false, error: "The move already stopped." }]);
  fail = false;
  expect(await a.retry()).toBe(true);
  expect(calls).toEqual(["h1", "h1"]);
  expect(states.at(-1)).toEqual({ busy: false });
});

test("no answer says so; a cancelled call says nothing", async () => {
  const states: ActionState[] = [];
  const slow = createAction(async () => { throw new ApiError(0, "The server took too long to answer."); }, (s) => states.push(s));
  await slow.run();
  expect(states.at(-1)).toEqual({ busy: false, error: "The server took too long to answer." });
  const gone = createAction(async () => { throw new Aborted(); }, (s) => states.push(s));
  await gone.run();
  expect(states.at(-1)).toEqual({ busy: false });
});

test("after runs once it worked, and its failure counts as the action's", async () => {
  const states: ActionState[] = [];
  const seen: number[] = [];
  const a = createAction(async (n: number) => n * 2, (s) => states.push(s), (r) => { seen.push(r); });
  await a.run(4);
  expect(seen).toEqual([8]);
  const b = createAction(async () => 1, (s) => states.push(s), () => { throw new Error("Saved, but the list didn't refresh."); });
  expect(await b.run()).toBe(false);
  expect(states.at(-1)?.error).toBe("Saved, but the list didn't refresh.");
});
