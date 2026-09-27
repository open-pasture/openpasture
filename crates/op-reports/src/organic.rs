//! `organic_season`: per herd, the days on pasture in the season (the report's
//! dates) against the organic rule's 120, and — once the herd has a mean
//! weight and the feed log covers the season — the share of dry matter from
//! pasture against 30 %: demand = head × weight × intake per day, pasture =
//! demand − supplemental dry matter.

use chrono::{Duration, NaiveDate};
use op_core::Ctx;
use serde_json::{Value, json};

use crate::feed_log::{self, Filter};
use crate::history::{self, Farm, n};
use crate::{Column, Report, ReportDoc, ReportParams, ReportSection};

pub struct OrganicSeason;

pub const MIN_DAYS: usize = 120;
pub const MIN_PASTURE_SHARE: f64 = 30.0;
/// The feed log must reach this close to the season's ends.
const COVER_DAYS: i64 = 7;

/// Whether entries on these dates cover `[from, to]`: one in the first week
/// and one in the last.
pub fn covers(dates: &[NaiveDate], from: NaiveDate, to: NaiveDate) -> bool {
    let (Some(first), Some(last)) = (dates.iter().min(), dates.iter().max()) else { return false };
    *first < from + Duration::days(COVER_DAYS) && *last > to - Duration::days(COVER_DAYS)
}

fn yes(ok: bool) -> Value {
    json!(if ok { "Yes" } else { "No" })
}

#[async_trait::async_trait]
impl Report for OrganicSeason {
    fn id(&self) -> &'static str {
        "organic_season"
    }

    fn title(&self) -> &'static str {
        "Organic grazing season"
    }

    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc> {
        let farm = Farm::load(ctx).await?;
        let stays = farm.stays();
        let (w0, w1) = farm.window(p);
        let mut doc = history::doc(&farm, self.id(), self.title(), p, None);
        let f = farm.fmt;
        // The season as far as it has gone.
        let end = p.to.min(farm.today()).max(p.from);
        let season_days = if w1 > w0 { (end - p.from).num_days() + 1 } else { 0 };

        let herds: Vec<String> = farm.herd_ids(p).into_iter().filter(|h| farm.herds.get(h).is_some_and(|s| history::herd_days(s, w0, w1) > 0.0)).collect();

        let mut rows = Vec::new();
        let mut dmi_rows = Vec::new();
        let mut missing_inputs = false;
        for h in &herds {
            let own: Vec<_> = stays.iter().filter(|s| s.herd_id == *h).collect();
            let on = history::days_in_paddocks(&farm, &own, w0, w1);
            rows.push(vec![json!(farm.herd_name(h)), json!(season_days), json!(on), yes(on >= MIN_DAYS)]);

            let inputs = farm.inputs.herd(h);
            let feed = feed_log::list(ctx, &Filter { herd_id: Some(h.clone()), from: Some(p.from), to: Some(end) }).await?;
            let dates: Vec<NaiveDate> = feed.iter().map(|e| e.date).collect();
            match inputs.mean_weight_kg {
                Some(weight) if covers(&dates, p.from, end) => {
                    let head_days = history::herd_days(&farm.herds[h], w0, w1);
                    let demand = head_days * weight * inputs.intake_pct / 100.0;
                    let supplement: f64 = feed.iter().map(|e| e.kg_dm).sum();
                    let pasture = (demand - supplement).max(0.0);
                    let share = if demand > 0.0 { pasture / demand * 100.0 } else { 0.0 };
                    let mass = |kg: f64| n(f.convert("mass", kg), 0);
                    dmi_rows.push(vec![
                        json!(farm.herd_name(h)),
                        n(head_days, 1),
                        mass(weight),
                        n(inputs.intake_pct, 1),
                        mass(demand),
                        mass(supplement),
                        mass(pasture),
                        n(share, 1),
                        yes(share >= MIN_PASTURE_SHARE),
                    ]);
                }
                _ => missing_inputs = true,
            }
        }
        doc.sections.push(ReportSection {
            title: "Days on pasture".into(),
            columns: vec![
                Column::new("herd", "Herd"),
                Column::new("season_days", "Days in season").dp(0),
                Column::new("pasture_days", "Days on pasture").dp(0),
                Column::new("min_days", "At least 120"),
            ],
            rows,
            totals: None,
        });
        if !dmi_rows.is_empty() {
            let mass = f.unit_label("mass");
            doc.sections.push(ReportSection {
                title: "Dry matter from pasture".into(),
                columns: vec![
                    Column::new("herd", "Herd"),
                    Column::new("head_days", "Head-days").dp(1),
                    Column::unit("weight", "Mean weight", mass).dp(0),
                    Column::unit("intake", "Intake", "%").dp(1),
                    Column::unit("demand", "Dry matter needed", mass).dp(0),
                    Column::unit("supplement", "Supplement fed", mass).dp(0),
                    Column::unit("pasture", "From pasture", mass).dp(0),
                    Column::unit("share", "From pasture", "%").dp(1),
                    Column::new("min_share", "At least 30 %"),
                ],
                rows: dmi_rows,
                totals: None,
            });
        }

        doc.notes.push("Days on pasture: farm days on which the herd was in a paddock at any time.".into());
        doc.notes
            .push("The organic rule asks for at least 120 days on pasture in the grazing season and at least 30 % of dry matter from pasture over it.".into());
        if doc.sections.len() > 1 {
            doc.notes.push("Dry matter needed is head-days × mean weight × daily intake; from pasture is that less the supplement in the feed log.".into());
        }
        if missing_inputs && !herds.is_empty() {
            doc.notes.push(
                "Dry matter from pasture shows for a herd once it has a mean weight and feed-log entries in the season's first and last week (0 counts)."
                    .into(),
            );
        }
        if stays.iter().any(|s| s.backfilled && herds.contains(&s.herd_id))
            && let Some(n) = history::backfill_note(&farm)
        {
            doc.notes.push(n);
        }
        Ok(doc)
    }
}
