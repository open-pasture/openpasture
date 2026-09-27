//! `nrcs_528`: the Prescribed Grazing (practice 528) record an NRCS planner
//! asks for: field and FSA numbers, dates in and out, kind and number of
//! livestock, AU, days, AUD and the rest period, with the operator's header and
//! signature lines. Columns openpasture doesn't measure are left out.

use std::collections::BTreeSet;

use op_core::Ctx;
use serde_json::{Value, json};

use crate::history::{self, Farm, n};
use crate::paddock_record::{common_notes, events};
use crate::{Column, Report, ReportDoc, ReportParams, ReportSection};

pub struct Nrcs528;

#[async_trait::async_trait]
impl Report for Nrcs528 {
    fn id(&self) -> &'static str {
        "nrcs_528"
    }

    fn title(&self) -> &'static str {
        "NRCS 528 grazing record"
    }

    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc> {
        let farm = Farm::load(ctx).await?;
        let stays = farm.stays();
        let events = events(ctx, &farm, &stays, p).await?;
        let fields: BTreeSet<String> = events.iter().map(|e| e.stay.paddock_id.clone()).collect();
        let shared = history::shared_fsa_farm(&farm, &fields);
        let mut doc = history::doc(&farm, self.id(), self.title(), p, shared.clone());
        let f = farm.fmt;

        let has = |key: &str| fields.iter().any(|p| farm.paddock_prop(p, key).is_some());
        // The FSA farm goes in the header when one number covers every field
        // (or the operator set one); a column only when fields differ.
        let fsa_farm_col = has("fsa_farm") && shared.is_none() && farm.inputs.settings.fsa_farm.is_none();
        let fsa = [("fsa_farm", "FSA farm", fsa_farm_col), ("fsa_tract", "FSA tract", has("fsa_tract")), ("fsa_field", "FSA field", has("fsa_field"))];

        let mut columns = vec![Column::new("field", "Field")];
        for (key, label, on) in fsa {
            if on {
                columns.push(Column::new(key, label));
            }
        }
        columns.extend([
            Column::unit("area", "Area", f.unit_label("area")).dp(1),
            Column::new("date_in", "Date in"),
            Column::new("date_out", "Date out"),
            Column::new("kind", "Kind"),
            Column::new("number", "Number").dp(0),
            Column::new("au", "AU").dp(1),
            Column::new("days", "Days").dp(1),
            Column::new("aud", "AUD").dp(1),
            Column::new("rest", "Rest period").dp(1),
        ]);
        // @S: planned vs actual days, residual height at exit.
        let extra = crate::schedule_cols::load(ctx, &events, farm.now).await?;
        let shown = crate::schedule_cols::shown(&extra);
        columns.extend(crate::schedule_cols::columns(&f, shown));

        let mut rows = Vec::new();
        let (mut days, mut aud) = (0.0, 0.0);
        for (i, e) in events.iter().enumerate() {
            let pid = &e.stay.paddock_id;
            let mut row = vec![json!(farm.paddock_name(pid))];
            for (key, _, on) in fsa {
                if on {
                    row.push(farm.paddock_prop(pid, key).map_or(Value::Null, Value::from));
                }
            }
            row.extend([
                e.area_ha.map_or(Value::Null, |a| n(f.convert("area", a), 1)),
                json!(farm.local_date(e.cut.start).to_string()),
                // A stay still running after the report's last day is out on that day, not the next.
                if e.cut.open { Value::Null } else { json!(e.cut.last_day(&farm).to_string()) },
                json!(history::species_label(&farm.species(&e.stay.herd_id))),
                json!(e.cut.head),
                n(e.au(), 1),
                n(e.cut.days, 1),
                n(e.au_days(), 1),
                e.rest_days.map_or(Value::Null, |r| n(r, 1)),
            ]);
            row.extend(crate::schedule_cols::cells(&f, &extra[i], shown));
            days += e.cut.days;
            aud += e.au_days();
            rows.push(row);
        }
        let totals = (!rows.is_empty()).then(|| {
            let mut t = vec![json!("Total")];
            t.resize(columns.len(), Value::Null);
            let at = |key: &str| columns.iter().position(|c| c.key == key).expect("column");
            t[at("days")] = n(days, 1);
            t[at("aud")] = n(aud, 1);
            t
        });
        doc.sections.push(ReportSection { title: "Grazing record".into(), columns, rows, totals });

        doc.notes = common_notes(&farm, &events, p);
        doc.notes.push("Number is the head count on the date in; AUD are animal-unit days.".into());
        doc.notes.push("Rest period: days since any herd last left the field before the date in.".into());
        doc.notes.extend(crate::schedule_cols::notes(shown));
        doc.signatures = vec!["Operator".into(), "NRCS planner".into()];
        Ok(doc)
    }
}
