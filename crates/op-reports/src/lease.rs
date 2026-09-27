//! `lease_head_days`: per landowner and leased paddock, the grazing inside the
//! lease season and the report's dates — head-days, AU-days, AUM, pair-months
//! when the herd's pairs are known — and what it comes to at the lease rate.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use op_core::Ctx;
use serde_json::{Value, json};

use crate::history::{self, Farm, n, round};
use crate::leases_api::{self, Lease, RatePer};
use crate::{Column, Report, ReportDoc, ReportParams, ReportSection};

pub struct LeaseHeadDays;

/// Days in an animal-unit month (and a pair-month).
pub const DAYS_PER_MONTH: f64 = 30.4;

/// Grazing on one leased paddock inside its window.
#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub head_days: f64,
    pub au_days: f64,
    /// None when a herd that grazed has no known pairs.
    pub pair_days: Option<f64>,
    pub area_ha: f64,
}

impl Use {
    pub fn aum(&self) -> f64 {
        self.au_days / DAYS_PER_MONTH
    }

    pub fn pair_months(&self) -> Option<f64> {
        self.pair_days.map(|d| d / DAYS_PER_MONTH)
    }

    /// What the grazing comes to at the lease's rate (per hectare for
    /// `acre_season`); None for pair-months without known pairs.
    pub fn amount(&self, l: &Lease) -> Option<f64> {
        Some(match l.rate_per {
            RatePer::AcreSeason => l.rate_amount * self.area_ha,
            RatePer::HeadDay => l.rate_amount * self.head_days,
            RatePer::AuDay => l.rate_amount * self.au_days,
            RatePer::Aum => l.rate_amount * self.aum(),
            RatePer::PairMonth => l.rate_amount * self.pair_months()?,
        })
    }
}

/// The lease's window inside the report's, or None when they don't meet.
fn window(farm: &Farm, l: &Lease, p: &ReportParams) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (w0, w1) = farm.window(p);
    let a = l.season_from.map_or(w0, |d| farm.midnight(d).max(w0));
    let z = l.season_to.map_or(w1, |d| farm.midnight(d + Duration::days(1)).min(w1));
    (z > a).then_some((a, z))
}

/// Grazing on the lease's paddock in its window, from every herd (or the one asked for).
pub fn usage(farm: &Farm, stays: &[history::Stay], l: &Lease, p: &ReportParams) -> Option<Use> {
    let (a, z) = window(farm, l, p)?;
    let mut u = Use { start: a, end: z, head_days: 0.0, au_days: 0.0, pair_days: Some(0.0), area_ha: farm.area_at(&l.paddock_id, a).unwrap_or(0.0) };
    for s in stays.iter().filter(|s| s.paddock_id == l.paddock_id && p.herd_id.as_ref().is_none_or(|h| *h == s.herd_id)) {
        let Some(c) = s.cut(a, z, farm.now) else { continue };
        u.head_days += c.head_days;
        u.au_days += c.head_days * farm.au_per_head(&s.herd_id);
        u.pair_days = match (u.pair_days, farm.pair_share(&s.herd_id)) {
            (Some(d), Some(share)) => Some(d + c.head_days * share),
            _ => None,
        };
    }
    Some(u)
}

/// "45.00 per ac, season", in the farm's units.
fn rate_text(farm: &Farm, l: &Lease) -> String {
    match l.rate_per {
        RatePer::AcreSeason => {
            // Per hectare in the record; per the farm's area unit here.
            let per = l.rate_amount / farm.fmt.convert("area", 1.0);
            format!("{per:.2} per {}, season", farm.fmt.unit_label("area"))
        }
        RatePer::HeadDay => format!("{:.2} per head-day", l.rate_amount),
        RatePer::AuDay => format!("{:.2} per AU-day", l.rate_amount),
        RatePer::Aum => format!("{:.2} per AUM", l.rate_amount),
        RatePer::PairMonth => format!("{:.2} per pair-month", l.rate_amount),
    }
}

#[async_trait::async_trait]
impl Report for LeaseHeadDays {
    fn id(&self) -> &'static str {
        "lease_head_days"
    }

    fn title(&self) -> &'static str {
        "Lease head-days"
    }

    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc> {
        let farm = Farm::load(ctx).await?;
        let stays = farm.stays();
        let leases = leases_api::all(ctx).await?;
        let used: Vec<(Lease, Use)> = leases.into_iter().filter_map(|l| usage(&farm, &stays, &l, p).map(|u| (l, u))).collect();
        let paddocks: BTreeSet<String> = used.iter().map(|(l, _)| l.paddock_id.clone()).collect();
        let mut doc = history::doc(&farm, self.id(), self.title(), p, history::shared_fsa_farm(&farm, &paddocks));
        let f = farm.fmt;

        let pairs = used.iter().any(|(l, u)| l.rate_per == RatePer::PairMonth || u.pair_days.is_some_and(|d| d > 0.0));
        let mut by: BTreeMap<String, Vec<&(Lease, Use)>> = BTreeMap::new();
        for x in &used {
            by.entry(x.0.landowner.clone()).or_default().push(x);
        }
        for (landowner, mut list) in by {
            list.sort_by_key(|(l, _)| farm.paddock_name(&l.paddock_id));
            let currencies: BTreeSet<&str> = list.iter().map(|(l, _)| l.currency.as_str()).collect();
            let one_currency = (currencies.len() == 1).then(|| currencies.iter().next().map(|c| c.to_string())).flatten();
            let mut columns = vec![
                Column::new("paddock", "Paddock"),
                Column::unit("area", "Area", f.unit_label("area")).dp(1),
                Column::new("dates", "Dates"),
                Column::new("head_days", "Head-days").dp(1),
                Column::new("au_days", "AU-days").dp(1),
                Column::new("aum", "AUM").dp(1),
            ];
            if pairs {
                columns.push(Column::new("pair_months", "Pair-months").dp(1));
            }
            columns.push(Column::new("rate", "Rate"));
            columns.push(match &one_currency {
                Some(c) => Column::unit("amount", "Amount", c.clone()).dp(2),
                None => Column::new("amount", "Amount"),
            });

            let mut rows = Vec::new();
            let (mut area, mut hd, mut ad, mut pm, mut amount) = (0.0, 0.0, 0.0, Some(0.0), Some(0.0));
            for (l, u) in list {
                let dates = format!("{} – {}", farm.local_date(u.start), farm.local_date(u.end - Duration::milliseconds(1)));
                let money = u.amount(l).map(|a| round(a, 2));
                let mut row = vec![
                    json!(farm.paddock_name(&l.paddock_id)),
                    n(f.convert("area", u.area_ha), 1),
                    json!(dates),
                    n(u.head_days, 1),
                    n(u.au_days, 1),
                    n(u.aum(), 1),
                ];
                if pairs {
                    row.push(u.pair_months().map_or(Value::Null, |m| n(m, 1)));
                }
                row.push(json!(rate_text(&farm, l)));
                row.push(match (money, &one_currency) {
                    (None, _) => Value::Null,
                    (Some(a), Some(_)) => json!(a),
                    (Some(a), None) => json!(format!("{a:.2} {}", l.currency)),
                });
                rows.push(row);
                area += u.area_ha;
                hd += u.head_days;
                ad += u.au_days;
                pm = pm.zip(u.pair_months()).map(|(a, b)| a + b);
                amount = amount.zip(money).map(|(a, b)| a + b);
            }
            let mut totals = vec![json!("Total"), n(f.convert("area", area), 1), Value::Null, n(hd, 1), n(ad, 1), n(ad / DAYS_PER_MONTH, 1)];
            if pairs {
                totals.push(pm.map_or(Value::Null, |m| n(m, 1)));
            }
            totals.push(Value::Null);
            totals.push(match (amount, &one_currency) {
                (Some(a), Some(_)) => n(a, 2),
                _ => Value::Null,
            });
            doc.sections.push(ReportSection { title: landowner.clone(), columns, rows, totals: Some(totals) });
            doc.signatures.push(landowner);
        }
        if !doc.signatures.is_empty() {
            doc.signatures.insert(0, "Operator".into());
        }

        let herds: Vec<String> = stays
            .iter()
            .filter(|s| paddocks.contains(&s.paddock_id) && p.herd_id.as_ref().is_none_or(|h| *h == s.herd_id))
            .map(|s| s.herd_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Some(n) = history::au_note(&farm, &herds) {
            doc.notes.push(n);
        }
        if !used.is_empty() {
            doc.notes.push("Head-days count every herd in the paddock inside both the lease season and these dates. AUM: AU-days ÷ 30.4.".into());
        }
        if pairs {
            doc.notes.push("Pair-months: cow-calf pairs × days ÷ 30.4, for herds whose mix has pairs.".into());
        }
        if used.iter().any(|(l, _)| l.rate_per == RatePer::AcreSeason) {
            doc.notes.push("Rent per area is the whole season's, owed when the season meets these dates.".into());
        }
        if stays.iter().any(|s| s.backfilled && paddocks.contains(&s.paddock_id))
            && let Some(n) = history::backfill_note(&farm)
        {
            doc.notes.push(n);
        }
        Ok(doc)
    }
}
