//! `paddock_record`: every grazing event (a herd's stay in a paddock) in the
//! report's dates, with head-days, AU-days, stocking density and the rest the
//! paddock had before, then a line per paddock.

use std::collections::{BTreeMap, BTreeSet};

use op_core::Ctx;
use serde_json::{Value, json};

use crate::history::{self, Cut, Farm, Stay, n};
use crate::{Column, Report, ReportDoc, ReportParams, ReportSection};

pub struct PaddockRecord;

/// One grazing event with everything the paddock record and the NRCS 528
/// record show about it. Values are SI; the reports convert.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub stay: &'a Stay,
    pub cut: Cut,
    pub au_per_head: f64,
    /// AU on the first day, per hectare of the paddock then.
    pub au_per_ha: Option<f64>,
    pub area_ha: Option<f64>,
    pub rest_days: Option<f64>,
    pub collar_days: usize,
}

impl Event<'_> {
    pub fn au(&self) -> f64 {
        self.cut.head as f64 * self.au_per_head
    }

    pub fn au_days(&self) -> f64 {
        self.cut.head_days * self.au_per_head
    }
}

/// The grazing events in the report's dates, by time in (then paddock).
pub async fn events<'a>(ctx: &Ctx, farm: &Farm, stays: &'a [Stay], p: &ReportParams) -> anyhow::Result<Vec<Event<'a>>> {
    let (w0, w1) = farm.window(p);
    let dwell = history::collar_days(ctx, w0, w1).await?;
    let mut out = Vec::new();
    for s in stays.iter().filter(|s| p.herd_id.as_ref().is_none_or(|h| *h == s.herd_id)) {
        let Some(cut) = s.cut(w0, w1, farm.now) else { continue };
        let area_ha = farm.area_at(&s.paddock_id, cut.start);
        let au_per_head = farm.au_per_head(&s.herd_id);
        out.push(Event {
            stay: s,
            au_per_ha: area_ha.filter(|a| *a > 0.0).map(|a| cut.head as f64 * au_per_head / a),
            area_ha,
            rest_days: farm.rest_before(stays, &s.paddock_id, s.start),
            collar_days: history::collar_days_in(dwell.get(&(s.herd_id.clone(), s.paddock_id.clone())), &cut),
            au_per_head,
            cut,
        });
    }
    out.sort_by(|a, b| a.cut.start.cmp(&b.cut.start).then_with(|| farm.paddock_name(&a.stay.paddock_id).cmp(&farm.paddock_name(&b.stay.paddock_id))));
    Ok(out)
}

/// Method lines both grazing records share.
pub fn common_notes(farm: &Farm, events: &[Event], p: &ReportParams) -> Vec<String> {
    let mut notes = Vec::new();
    let herds: Vec<String> = events.iter().map(|e| e.stay.herd_id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    if let Some(n) = history::au_note(farm, &herds) {
        notes.push(n);
    }
    if events.iter().any(|e| e.cut.recounted) {
        notes.push("Head is the count on the day in; head-days follow every change in the count.".into());
    }
    if events.iter().any(|e| e.stay.backfilled)
        && let Some(n) = history::backfill_note(farm)
    {
        notes.push(n);
    }
    if events.iter().any(|e| e.cut.open) {
        notes.push("A blank out date means the herd is still there; its days run to now.".into());
    }
    if events.iter().any(|e| e.cut.cut) {
        notes.push(format!("Events are cut at {} and the end of {}.", p.from, p.to));
    }
    notes
}

#[async_trait::async_trait]
impl Report for PaddockRecord {
    fn id(&self) -> &'static str {
        "paddock_record"
    }

    fn title(&self) -> &'static str {
        "Paddock grazing record"
    }

    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc> {
        let farm = Farm::load(ctx).await?;
        let stays = farm.stays();
        let events = events(ctx, &farm, &stays, p).await?;
        let paddocks: BTreeSet<String> = events.iter().map(|e| e.stay.paddock_id.clone()).collect();
        let mut doc = history::doc(&farm, self.id(), self.title(), p, history::shared_fsa_farm(&farm, &paddocks));
        let f = farm.fmt;
        let area = |ha: f64| n(f.convert("area", ha), 1);

        let fsa_field = paddocks.iter().any(|p| farm.paddock_prop(p, "fsa_field").is_some());
        let collar = events.iter().any(|e| e.collar_days > 0);
        let mut columns = vec![Column::new("paddock", "Paddock")];
        if fsa_field {
            columns.push(Column::new("fsa_field", "FSA field"));
        }
        columns.extend([
            Column::new("herd", "Herd"),
            Column::new("in", "In"),
            Column::new("out", "Out"),
            Column::new("days", "Days"),
            Column::new("head", "Head"),
            Column::new("au", "AU"),
            Column::new("head_days", "Head-days"),
            Column::new("au_days", "AU-days"),
            Column::unit("density", "Stocking density", f.unit_label("density")),
            Column::new("rest_days", "Rest before in"),
        ]);
        if collar {
            columns.push(Column::new("collar_days", "Collar days"));
        }

        let mut rows = Vec::new();
        let (mut days, mut head_days, mut au_days, mut collar_days) = (0.0, 0.0, 0.0, 0usize);
        for e in &events {
            let mut row = vec![json!(farm.paddock_name(&e.stay.paddock_id))];
            if fsa_field {
                row.push(farm.paddock_prop(&e.stay.paddock_id, "fsa_field").map_or(Value::Null, Value::from));
            }
            row.extend([
                json!(farm.herd_name(&e.stay.herd_id)),
                json!(farm.local_time(e.cut.start)),
                if e.cut.open { Value::Null } else { json!(farm.local_time(e.cut.end)) },
                n(e.cut.days, 1),
                json!(e.cut.head),
                n(e.au(), 1),
                n(e.cut.head_days, 1),
                n(e.au_days(), 1),
                e.au_per_ha.map_or(Value::Null, |d| n(f.convert("density", d), 1)),
                e.rest_days.map_or(Value::Null, |r| n(r, 1)),
            ]);
            if collar {
                row.push(json!(e.collar_days));
            }
            days += e.cut.days;
            head_days += e.cut.head_days;
            au_days += e.au_days();
            collar_days += e.collar_days;
            rows.push(row);
        }
        let mut totals = vec![json!("Total")];
        totals.resize(columns.len(), Value::Null);
        let at = |key: &str| columns.iter().position(|c| c.key == key).expect("column");
        totals[at("days")] = n(days, 1);
        totals[at("head_days")] = n(head_days, 1);
        totals[at("au_days")] = n(au_days, 1);
        if collar {
            totals[at("collar_days")] = json!(collar_days);
        }
        let totals = (!rows.is_empty()).then_some(totals);
        doc.sections.push(ReportSection { title: "Grazing events".into(), columns, rows, totals });

        // By paddock, in name order.
        let mut by: BTreeMap<String, (String, usize, f64, f64, f64)> = BTreeMap::new();
        for e in &events {
            let name = farm.paddock_name(&e.stay.paddock_id);
            let row = by.entry(format!("{name}\u{0}{}", e.stay.paddock_id)).or_insert((e.stay.paddock_id.clone(), 0, 0.0, 0.0, 0.0));
            row.1 += 1;
            row.2 += e.cut.days;
            row.3 += e.cut.head_days;
            row.4 += e.au_days();
        }
        let columns = vec![
            Column::new("paddock", "Paddock"),
            Column::unit("area", "Area", f.unit_label("area")),
            Column::new("events", "Events"),
            Column::new("days", "Days"),
            Column::new("head_days", "Head-days"),
            Column::new("au_days", "AU-days"),
        ];
        let mut sum = (0.0, 0usize, 0.0, 0.0, 0.0);
        let rows: Vec<Vec<Value>> = by
            .values()
            .map(|(id, count, days, hd, ad)| {
                let a = farm.area_now(id).unwrap_or(0.0);
                sum = (sum.0 + a, sum.1 + count, sum.2 + days, sum.3 + hd, sum.4 + ad);
                vec![json!(farm.paddock_name(id)), area(a), json!(count), n(*days, 1), n(*hd, 1), n(*ad, 1)]
            })
            .collect();
        let totals = (!rows.is_empty()).then(|| vec![json!("Total"), area(sum.0), json!(sum.1), n(sum.2, 1), n(sum.3, 1), n(sum.4, 1)]);
        doc.sections.push(ReportSection { title: "By paddock".into(), columns, rows, totals });

        doc.notes = common_notes(&farm, &events, p);
        doc.notes.push("Stocking density: animal units on the day in over the paddock's area then.".into());
        doc.notes.push("Rest before in: days since any herd last left the paddock.".into());
        if collar {
            doc.notes.push("Collar days: days on which collar positions place the herd in the paddock.".into());
        }
        Ok(doc)
    }
}
