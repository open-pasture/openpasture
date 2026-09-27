---
name: daily-grazing-decision
description: Run one grazing decision for one herd in one decision window. Assemble context, read the signals, consult knowledge, decide STAY, MOVE, NEEDS_INFO (or HOLD on a strip schedule), submit it, handle the farmer's response, confirm the boundary with the collars, and evaluate the outcome later.
version: 1.0.0
---
# Daily Grazing Decision

## When to Use

Use this skill every decision window for every herd. The window is daily by
default. Use it when the scheduler starts the daily decision, when the farmer
presses Decide in the app, when the farmer asks "should they move today?", or
when a new observation could change today's call.

One run of this skill produces one decision for one herd. If the farm
has three herds, run it three times.

## The Job

Answer one question: should this herd move today, and if so, where?

The answer is one of:

- `STAY`: the herd stays inside its current boundary for this window.
- `MOVE`: the herd moves to a specific land unit and a specific boundary.
- `NEEDS_INFO`: the call depends on something the data cannot tell you. Name
  the one observation that would settle it.
- `HOLD`: only while the herd is on a strip schedule (see "When The Herd Is On
  A Strip Schedule"). The herd stays on today's strip one more cadence.

You are working alongside the farmer. You act only within the autonomy the
farmer set. You say what was sent to the collars and whether it was confirmed.

## Procedure

### 1. Assemble

The engine assembles the context for the herd and hands it to you with these
instructions. It holds:

- the current paddock and boundary, and the candidates the herd could move to,
- the collar summary since the last decision: fixes, cues, battery, and which
  collars have gone quiet,
- a land report for the current paddock and each candidate,
- the farm's grazing history: last grazed dates, rest days, past residuals,
- the farmer's paddock notes,
- the last decisions for this herd, the farmer's responses, and their outcomes,
- the herd's autonomy setting.

An outside agent working through MCP builds the same picture with `get_farm`,
`get_herd`, `list_paddocks`, `get_boundary_status`, and `list_decisions`.

Before you reason, check freshness. Note anything stale or missing:

- land report sections that came back `status: "unavailable"`,
- imagery older than about 10 days, or cloudy,
- collars that have not reported since the last window,
- no farmer observation in the last several days.

Missing data is not a reason to hide the gap. It is a reason to lower
confidence or ask for one observation.

To look deeper, call `get_land_report` for one paddock, `get_herd_positions`
for where each collar is now, or `run_sql` for anything in the raw record.

### 2. Read The Signals

Call `get_signals` if the context does not already include them.
Read each signal as evidence, not as an answer:

- **Grazing pressure.** Animal-days per acre in the current paddock from
  collar positions. High pressure plus days in the paddock points to low
  residual.
- **Rest.** Days since each candidate was last grazed.
- **Forage.** Standing forage estimates from imagery and farmer notes. NDVI is
  a proxy for green cover, not grass height.
- **Recovery.** How fast each paddock regrew after past grazes. This is farm
  history, and it is often the best evidence you have.
- **Feed budget.** Days of grazing ahead given forage, herd demand, and the
  forecast.
- **Risk.** Heat, cold, heavy rain, drought, flood, and access risk from
  weather and hazards.
- **Behavior.** Time grazing versus resting, and cue counts on the boundary.
  Rising cues and pressure on one edge often mean the animals want out.

### 3. Consult Knowledge

Call `search_knowledge` with the specific situation, not a generic query. For
example: "residual before heavy rain", "moving cattle onto lush regrowth",
"rest period in late summer slump". Prefer lessons that match the season,
species, and condition. Keep the ids of the lessons you actually used so the
reasoning can cite them.

### 4. Decide

Work through the grazing judgment below. Then pick the action.

- Prefer the simple, explainable call over a clever one.
- A `MOVE` needs a target land unit and a boundary the herd can actually reach
  and live in: water, no hazards inside it, fence line clear of exclusions.
- If two candidates are close, prefer the one with better water access and the
  easier walk.
- If the answer turns on one fact you cannot get from data, choose
  `NEEDS_INFO`. Do not guess a `MOVE` to look useful.
- If yesterday's decision was overridden, read the farmer's note first. That
  override is the strongest signal you have about this paddock.

Set confidence plainly: `high`, `medium`, or `low`. The output carries it as a
number: about 0.85 for high, 0.6 for medium, 0.3 for low.

- `high`: signals agree, data is fresh, and the farm's history backs it up.
- `medium`: signals mostly agree but something is stale or thin.
- `low`: signals disagree or key data is missing. Usually this means
  `NEEDS_INFO`.

### 5. Write Reasoning A Farmer Trusts

Write two to four short lines. Each line is one reason tied to a fact.

- Lead with the fact that drives the call.
- Name the paddock, the number, and the source: "Paddock 4 has rested 38
  days", "12 mm of rain expected tomorrow", "collars show the herd pressing
  the north line since yesterday".
- Say what you assumed and what is stale.
- Cite a lesson only when it actually shaped the call.
- No jargon a farmer would not use. No hedging filler.

Good:

```text
Paddock 3 is at roughly 4 inches of residual by collar pressure and imagery.
Paddock 4 has rested 38 days and imagery shows full recovery.
12 mm of rain is expected tomorrow. Moving today keeps the herd off wet ground on the low side of 3.
```

Weak:

```text
Based on a comprehensive analysis of multiple data sources, conditions appear favorable for a rotation.
```

### 6. Pick The Single Most Useful Observation

Ask for one observation only when the answer to it could change the call. Pick
the one that would change it most. Make it something the farmer can check in a
few minutes on the way past.

- "How tall is the grass at the gate end of Paddock 3? Under 4 inches means
  move today."
- "Is the tank in Paddock 5 full and the float working?"
- "Can you drive to the south end of 6 without rutting after last night's
  rain?"

Do not ask for a list. Do not ask for something the data already answered.

### 7. Submit

Return the decision in the output schema: the action, `to_paddock_id` for a
`MOVE` (and `geometry` when the boundary is not the whole paddock), the
reasoning, the confidence, and `need`, the one observation request. The engine
records it on the herd's decision. An outside agent working through MCP
proposes a move with `propose_boundary`, which records a decision the same way.

What happens next depends on the herd's autonomy setting:

- `propose` (default): the decision waits for the farmer. Nothing is sent.
- `timer`: the boundary is sent after the herd's timer runs out unless the
  farmer stops it first.
- `auto`: the boundary is sent now. The farmer reviews after.

Tell the farmer which of these applies. Never describe a proposed move as done.

For a `MOVE`, include the lane in the boundary when the walk is long or the
herd is nervous, so animals walking the right way move without being cued. Use
a tight boundary only for a short, familiar move.

### 8. Handle The Farmer's Response

The farmer answers in the app, from the farm view: approve, modify, or reject.

- **Approved.** The boundary is sent as proposed.
- **Modified.** The farmer's boundary is the one sent. Yours stays on the
  record. Read the note and carry it forward, for example "the wet spot is
  bigger than it looks" belongs in the paddock's history.
- **Rejected.** Nothing is sent. Ask once, briefly, what they saw, only if you
  do not already know. Record it.

An override is never an error. Do not argue it or re-propose the same move in
the same window. Treat it as the most useful signal of the day.

If the farmer answers you in chat, confirm what you understood and point them
to the decision in the app. Only the farmer approves, changes, or rejects it.

### 9. Confirm The Boundary

After a boundary is sent, call `get_boundary_status` for the herd.

- A boundary is active only once enough collars confirm it.
- Tell the farmer plainly: sent to how many collars, how many applied, which
  did not confirm, and any `rejected` reasons.
- If collars have not confirmed after a reasonable wait, say so. Name the
  collars. Suggest a quick check: battery, signal, or an animal that is out of
  range. Do not call the move done.

Example:

```text
Sent the Paddock 4 boundary (version 42) to 28 collars at 7:30.
26 applied. oc_0017 and oc_0021 have not confirmed; both were low on battery yesterday.
```

### 10. Evaluate The Outcome

On later cycles the engine evaluates past decisions that are due and writes the
outcome onto each one: whether the herd held the boundary, cue counts, and
recovery in later imagery. Read it in the context's history, or with
`get_decision` and `list_decisions`.

Read the outcome of the last decision before making the next one. If the herd
kept pressing a line, if residual came in lower than you estimated, or if a
paddock recovered slower than planned, adjust today's call and say why.

## When The Herd Is On A Strip Schedule

When the context has a `schedule` with `status: "active"`, the herd is being
walked across a paddock's strips on a cadence, and each open is already staged
on the collars. Today's call is about the schedule, not about a new paddock:

- `STAY` keeps the schedule: the next strip opens on time (`schedule.next`:
  strip k of n, `opens` in farm time, and how many collars already store it).
- `HOLD` repeats today's strip: the next open and everything after it move one
  cadence later, and the collars drop the staged open. Hold when today's strip
  still has grass (`schedule.today.days` is the grazing it holds for the herd;
  a fresh field note beats it), or when weather says don't move today.
- `MOVE` to another paddock ends the schedule once the move applies. Only move
  when the paddock as a whole is done or unsafe.

The farmer's text answers the same way: `Y` keeps the schedule, `N` holds
today's strip. In the reasoning, name the strip and when it opens: "Strip 4 of
12 opens 07:00; strip 3 is grazed down to 4 inches."

## Grazing Judgment

These are defaults. The farm's own history and the farmer's word beat them.

- **Residual.** Leave enough behind for the plant to regrow fast. For most
  cool- and warm-season pastures that means taking the top third to half and
  leaving several inches. Move before the herd starts eating into the base.
  Low residual costs you weeks of recovery.
- **Rest.** Do not return to a paddock before it has recovered. Rest needs
  change with the season: short in fast spring growth, long in summer slump or
  drought. Base it on recovery, not the calendar.
- **Recovery.** Read how each paddock regrew last time. A paddock that took 50
  days to recover in August will not be ready in 30 this August.
- **Weather timing.** Move ahead of heavy rain so the herd is not trampling
  wet ground on a grazed-down paddock. In heat, favor shade and water. Before a
  cold snap, favor shelter and standing forage.
- **Wet ground.** Keep the herd off low, saturated ground after rain. Pugging
  and rutting damage lasts longer than one graze. Exclude wet spots and creek
  banks in the boundary when you can.
- **Water access.** Every boundary needs reliable water inside it or through a
  lane. No water, no move.
- **Transitions.** Keep walks short and familiar. Use a corridor for longer
  moves. Watch for bloat risk when moving hungry cattle onto lush legume
  regrowth; move them full, or later in the day.
- **Animals.** Behavior is evidence. Bawling, pacing the line, rising cues, and
  less time grazing say the paddock is done before imagery does.
- **Labor.** A move the farmer cannot check on is a worse move. Respect the
  farmer's schedule when timing matters.

## Pitfalls

- Do not present a proposed move as sent, or a sent move as active.
- Do not act beyond the herd's autonomy setting.
- Do not hide stale or missing data behind a confident tone.
- Do not trust imagery over a fresh field note that contradicts it.
- Do not ask for more than one observation.
- Do not re-propose a move the farmer just rejected in the same window.
- Do not cite knowledge that did not shape the call.
- Do not skip the last outcome. The decision record is how this gets better.

## Verification

A good run leaves a decision on the record that answers, in plain words: what
the engine saw, what it decided and why, what the farmer did, what the collars
did, and later, what the land did.
