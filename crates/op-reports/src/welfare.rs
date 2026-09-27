//! `welfare` (H): what the collars played to each animal, for a vet, a buyer
//! or a certifier. Per animal: cues by kind, seconds of tone (a day on
//! average and the most in one day), the longest episode, the loudest level,
//! episodes by how they ended, trained or learning and since when, fit checks
//! and times the collar lay still; then the farm days that had cues. Read
//! through op-analytics' welfare record, so it matches the app.

use std::collections::{BTreeSet, HashMap};

use op_analytics::welfare::{self, Cue, Episode, Whose};
use op_core::Ctx;
use serde_json::{Value, json};

use crate::history::{self, Farm, n};
use crate::{Column, Report, ReportDoc, ReportParams, ReportSection};

pub struct WelfareRecord;

/// The one place openpasture names what the collars don't have.
pub const AUDIO_ONLY_NOTE: &str = "The collars are audio only: they play a warning tone and have no stimulus of any kind.";

#[async_trait::async_trait]
impl Report for WelfareRecord {
    fn id(&self) -> &'static str {
        "welfare"
    }

    fn title(&self) -> &'static str {
        "Welfare record"
    }

    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc> {
        let farm = Farm::load(ctx).await?;
        let (w0, w1) = farm.window(p);
        let (a, z) = (w0.timestamp_millis(), w1.timestamp_millis());
        let mut doc = history::doc(&farm, self.id(), self.title(), p, None);
        let tz = welfare::farm_tz(ctx).await?;

        // Every animal of the herd (or the farm) that was still on the farm when the dates began.
        let mut animals: Vec<op_core::Animal> =
            ctx.store().list_animals(p.herd_id.as_deref()).await?.into_iter().filter(|x| x.removed_at.is_none_or(|r| r >= w0)).collect();
        animals.sort_by(|x, y| welfare::tag_order(&x.tag, &y.tag));
        let ids: Vec<String> = animals.iter().map(|x| x.id.clone()).collect();
        let cues = welfare::cues(ctx, Whose::Animals(&ids), a, z).await?;
        let eps = welfare::episodes(ctx, Whose::Animals(&ids), a, z).await?;
        let mut hist = welfare::history(ctx, &ids).await?;
        let trainings = welfare::training::all(ctx).await?;
        let spells = welfare::still_spells_all(ctx, w0, w1).await?;
        let worn = welfare::collars_by_animal(&cues, &eps);

        let mut cues_of: HashMap<&str, Vec<Cue>> = HashMap::new();
        for c in &cues {
            if let Some(id) = c.animal_id.as_deref() {
                cues_of.entry(id).or_default().push(c.clone());
            }
        }
        let mut eps_of: HashMap<&str, Vec<Episode>> = HashMap::new();
        for e in &eps {
            if let Some(id) = e.animal_id.as_deref() {
                eps_of.entry(id).or_default().push(e.clone());
            }
        }

        let one_herd = p.herd_id.is_some();
        let day_count = welfare::days(&tz, a, z.max(a + 1), &[], &[]).len().max(1) as f64;
        let (mut cue_rows, mut learn_rows, mut care_rows) = (Vec::new(), Vec::new(), Vec::new());
        let mut tot = (0u32, 0u32, 0.0f64);
        let mut any_derived = false;
        let mut any_legacy_tone = false;
        let mut n_used: BTreeSet<u32> = BTreeSet::new();
        for x in &animals {
            let mine_c = cues_of.remove(x.id.as_str()).unwrap_or_default();
            let mine_e = eps_of.remove(x.id.as_str()).unwrap_or_default();
            let days = welfare::days(&tz, a, z.max(a + 1), &mine_c, &mine_e);
            let warn = mine_c.iter().filter(|c| c.kind == "warn").count() as u32;
            let outside = mine_c.len() as u32 - warn;
            let tone: f64 = mine_c.iter().map(|c| c.tone_ms() as f64 / 1000.0).sum();
            any_legacy_tone |= mine_c.iter().any(|c| c.dur_ms.is_none());
            let most = days.iter().map(|d| d.tone_s).fold(0.0, f64::max);
            let longest = mine_e.iter().map(Episode::secs).fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))));
            let loudest = mine_c.iter().filter(|c| c.kind == "warn").map(|c| c.level).max();
            tot = (tot.0 + warn, tot.1 + outside, tot.2 + tone);

            let mut row = vec![json!(x.tag)];
            if !one_herd {
                row.push(json!(farm.herd_name(&x.herd_id)));
            }
            row.extend([
                json!(warn),
                json!(outside),
                n(tone, 1),
                n(tone / day_count, 1),
                n(most, 1),
                longest.map_or(Value::Null, |s| n(s, 1)),
                loudest.map_or(Value::Null, |l| json!(l)),
            ]);
            cue_rows.push(row);

            let trained_after = trainings.get(&x.herd_id).copied().unwrap_or_default().trained_after;
            n_used.insert(trained_after);
            let l = welfare::learning(&hist.remove(&x.id).unwrap_or_default(), trained_after, Some(z));
            let mut o = welfare::Outcomes::default();
            for e in &mine_e {
                o.add(&e.outcome);
            }
            let derived = mine_e.iter().any(|e| e.derived);
            any_derived |= derived;
            learn_rows.push(vec![
                json!(x.tag),
                json!(o.turned_back),
                json!(o.crossed),
                json!(o.rest),
                json!(o.boundary_changed),
                l.status.map_or(Value::Null, |s| json!(s.word())),
                l.since.map_or(Value::Null, |t| json!(farm.local_date(t).to_string())),
                if derived { json!("Yes") } else { Value::Null },
            ]);

            let mut collars: BTreeSet<String> = worn.get(&x.id).cloned().unwrap_or_default();
            collars.extend(x.collar_id.clone());
            let collars: Vec<String> = collars.into_iter().collect();
            let checks = welfare::fit_checks(ctx, &collars, Some(w0), Some(w1)).await?;
            let still = welfare::spells_for(&spells, &x.id, &collars);
            care_rows.push(vec![
                json!(x.tag),
                json!(checks.len()),
                checks.first().map_or(Value::Null, |c| json!(farm.local_date(c.checked_at).to_string())),
                json!(still.len()),
            ]);
        }

        let mut cue_cols = vec![Column::new("tag", "Tag")];
        if !one_herd {
            cue_cols.push(Column::new("herd", "Herd"));
        }
        cue_cols.extend([
            Column::new("warn", "Warn cues"),
            Column::new("outside", "Outside cues"),
            Column::unit("tone", "Tone", "s").dp(1),
            Column::unit("tone_day", "Tone a day", "s").dp(1),
            Column::unit("tone_most", "Most in a day", "s").dp(1),
            Column::unit("longest", "Longest episode", "s").dp(1),
            Column::new("level", "Loudest level"),
        ]);
        let mut totals = vec![json!("Total")];
        if !one_herd {
            totals.push(Value::Null);
        }
        totals.extend([json!(tot.0), json!(tot.1), n(tot.2, 1), n(tot.2 / day_count, 1), Value::Null, Value::Null, Value::Null]);
        doc.sections.push(ReportSection { title: "Cues".into(), columns: cue_cols, rows: cue_rows, totals: (!animals.is_empty()).then_some(totals) });
        doc.sections.push(ReportSection {
            title: "Learning".into(),
            columns: vec![
                Column::new("tag", "Tag"),
                Column::new("turned_back", "Turned back"),
                Column::new("crossed", "Crossed"),
                Column::new("rest", "Rest"),
                Column::new("boundary_changed", "Boundary changed"),
                Column::new("status", "Status"),
                Column::new("since", "Since"),
                Column::new("derived", "From fixes"),
            ],
            rows: learn_rows,
            totals: None,
        });
        doc.sections.push(ReportSection {
            title: "Collar care".into(),
            columns: vec![
                Column::new("tag", "Tag"),
                Column::new("fit_checks", "Fit checks"),
                Column::new("last_fit", "Last fit check"),
                Column::new("still", "Collar lay still"),
            ],
            rows: care_rows,
            totals: None,
        });

        // The farm days that had cues, over every animal in the report.
        let mut day_rows = Vec::new();
        let all_days = welfare::days(&tz, a, z.max(a + 1), &cues, &eps);
        let mut cued: HashMap<chrono::NaiveDate, BTreeSet<&str>> = HashMap::new();
        for c in &cues {
            if let Some(id) = c.animal_id.as_deref() {
                cued.entry(welfare::local_date(&tz, c.t)).or_default().insert(id);
            }
        }
        for d in all_days.iter().filter(|d| d.warn + d.outside > 0) {
            day_rows.push(vec![
                json!(d.date.to_string()),
                json!(cued.get(&d.date).map_or(0, BTreeSet::len)),
                json!(d.warn),
                json!(d.outside),
                n(d.tone_s, 1),
                d.longest_s.map_or(Value::Null, |s| n(s, 1)),
            ]);
        }
        doc.sections.push(ReportSection {
            title: "Days with cues".into(),
            columns: vec![
                Column::new("date", "Date"),
                Column::new("animals", "Animals cued"),
                Column::new("warn", "Warn cues"),
                Column::new("outside", "Outside cues"),
                Column::unit("tone", "Tone", "s").dp(1),
                Column::unit("longest", "Longest episode", "s").dp(1),
            ],
            rows: day_rows,
            totals: None,
        });

        let mut notes = vec![
            AUDIO_ONLY_NOTE.to_owned(),
            "Warn: the warning tone, played in the warning zone inside the boundary and louder toward the line (level 1 to 4). Outside: a tone for at most 10 s after an animal crosses the line; the collar is then silent until the animal is back inside.".to_owned(),
            "An episode is a run of warning tones. It ends turned back (the animal went back inside), crossed, rest (after 20 s of warning the collar is silent for 30 s) or boundary changed (a new boundary arrived).".to_owned(),
        ];
        let n_text = n_used.iter().map(u32::to_string).collect::<Vec<_>>().join(" or ");
        notes.push(format!(
            "Trained: {} turned-back episodes in a row with no crossing; learning: any episode before that. Status is as of the end of the dates; since is the day it began.",
            if n_text.is_empty() { "5".to_owned() } else { n_text }
        ));
        if any_derived {
            notes.push("From fixes: the server rebuilt these episodes from the cues and fixes of firmware 0.1 collars, which don't report episodes.".into());
        }
        if any_legacy_tone {
            notes.push(format!(
                "Tone is the length the collars report; firmware 0.1 collars don't, so each of their cues counts {:.1} s.",
                welfare::LEGACY_BEEP_MS as f64 / 1000.0
            ));
        }
        notes.push("Collar lay still: times the collar stopped moving long enough to raise a not-moving alert; it may have come off.".into());
        notes.push(format!("Days are farm days ({}).", farm.tz));
        doc.notes = notes;
        doc.signatures = vec!["Operator".into()];
        Ok(doc)
    }
}
