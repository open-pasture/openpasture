//! Animals (K-animals): the rules every path that writes animals shares
//! (tags, EIDs, the head count that follows a herd's animals) and the
//! `list_animals` MCP tool.
//!
//! Head count: once a herd has any animal rows, `herds.count` is its active
//! animals (not removed). Every path that adds, removes or moves animals calls
//! [`sync_herd_count`]; a manual count on such a herd is refused.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{SqliteConnection, SqliteExecutor};

use crate::domain::{Animal, Collar, Sex};
use crate::error::{ApiError, ApiResult};
use crate::identity::Role;
use crate::tools::{ToolCall, ToolSpec};
use crate::{Ctx, store, time};

pub const MAX_TAG: usize = 64;
const MAX_NAME: usize = 200;
const MAX_BREED: usize = 100;
const MAX_NOTES: usize = 4000;

/// The 400 for a manual count on a herd whose count follows its animals.
pub const COUNT_FOLLOWS: &str = "Count follows the animals in this herd.";

/// A 15-digit electronic ID (ISO 11784) from what a person or a reader
/// typed. Spaces, dashes and dots are dropped: "982 000 123 456 789" and
/// "982-000123456789" are the same tag.
pub fn normalize_eid(s: &str) -> Result<String, String> {
    let digits: String = s.chars().filter(|c| !matches!(c, ' ' | '-' | '.' | '\u{a0}')).collect();
    if digits.len() == 15 && digits.bytes().all(|b| b.is_ascii_digit()) { Ok(digits) } else { Err(format!("EID {} isn't 15 digits.", s.trim())) }
}

/// A visual tag: trimmed, 1 to 64 characters, no control characters.
pub fn clean_tag(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("The animal needs a tag.".into());
    }
    if t.chars().count() > MAX_TAG {
        return Err(format!("Tag {t} is longer than {MAX_TAG} characters."));
    }
    if t.chars().any(char::is_control) {
        return Err("A tag can't hold control characters.".into());
    }
    Ok(t.to_owned())
}

/// Sex from the words herd records use: F, female, cow, heifer; M, male,
/// bull; steer, castrated, bullock.
pub fn parse_sex(s: &str) -> Option<Sex> {
    match s.trim().to_lowercase().as_str() {
        "f" | "female" | "cow" | "heifer" | "ewe" | "doe" | "nanny" => Some(Sex::Female),
        "m" | "male" | "bull" | "ram" | "buck" | "billy" | "intact" => Some(Sex::Male),
        "c" | "s" | "castrated" | "castrate" | "steer" | "bullock" | "wether" | "ox" => Some(Sex::Castrated),
        _ => None,
    }
}

/// A birth date as herd records write it: `2022-04-01`, `4/1/2022`
/// (month first when `month_first`, else day first), `4/1/22`,
/// `01.04.2022`, `2022/04/01`. A year alone isn't a date.
pub fn parse_born(s: &str, month_first: bool) -> Result<NaiveDate, String> {
    let t = s.trim();
    let bad = || format!("Birth date {t} isn't a date.");
    // Dotted dates (01.04.2022) are day first everywhere.
    let dotted = t.contains('.');
    // ISO first, with or without a time after it.
    let head = t.split(['T', ' ']).next().unwrap_or(t);
    if let Ok(d) = NaiveDate::parse_from_str(head, "%Y-%m-%d") {
        return Ok(d);
    }
    let parts: Vec<&str> = t.split(['/', '.', '-']).map(str::trim).collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return Err(bad());
    }
    let n: Vec<u32> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    let (y, m, d) = if parts[0].len() == 4 {
        (n[0] as i32, n[1], n[2])
    } else {
        let year = match parts[2].len() {
            4 => n[2] as i32,
            // Two-digit years are this century unless that is in the future.
            2 => {
                let this = (chrono::Utc::now().date_naive().format("%y").to_string().parse::<u32>().unwrap_or(0)) as i32;
                if n[2] as i32 > this { 1900 + n[2] as i32 } else { 2000 + n[2] as i32 }
            }
            _ => return Err(bad()),
        };
        let (m, d) = if dotted || !month_first { (n[1], n[0]) } else { (n[0], n[1]) };
        // "13/4/2022" can only be day first, "4/13/2022" only month first.
        let (m, d) = if m > 12 && d <= 12 { (d, m) } else { (m, d) };
        (year, m, d)
    };
    NaiveDate::from_ymd_opt(y, m, d).ok_or_else(bad)
}

/// Whether one file's slash birth dates are month first: what its dates that
/// read only one way show (13/4/2022 is day first, 4/13/2022 month first;
/// the more common when they disagree), else `default` (the farm's zone).
/// Read once per file, so 5/4/2022 means the same day on every row.
pub fn month_first_in<'a>(dates: impl IntoIterator<Item = &'a str>, default: bool) -> bool {
    let (mut day, mut month) = (0usize, 0usize);
    for t in dates {
        let t = t.trim();
        if t.contains('.') {
            continue;
        }
        let parts: Vec<&str> = t.split(['/', '-']).map(str::trim).collect();
        if parts.len() != 3 || parts[0].len() == 4 {
            continue;
        }
        let (Ok(a), Ok(b)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else { continue };
        if a > 12 && b <= 12 {
            day += 1;
        } else if b > 12 && a <= 12 {
            month += 1;
        }
    }
    match day.cmp(&month) {
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => default,
    }
}

fn tidy_text(v: &mut Option<String>, max: usize, what: &str) -> ApiResult<()> {
    if let Some(s) = v.take() {
        let s = s.trim();
        if s.chars().count() > max {
            return Err(ApiError::bad_request(format!("The {what} is longer than {max} characters.")));
        }
        if !s.is_empty() {
            *v = Some(s.to_owned());
        }
    }
    Ok(())
}

/// Trim and check an animal's own fields: tag, EID shape, name, breed,
/// notes, a birth date that isn't in the future. Blank optional fields
/// become absent.
pub fn tidy(a: &mut Animal) -> ApiResult<()> {
    a.tag = clean_tag(&a.tag).map_err(ApiError::bad_request)?;
    if let Some(e) = a.eid.take().filter(|e| !e.trim().is_empty()) {
        a.eid = Some(normalize_eid(&e).map_err(ApiError::bad_request)?);
    }
    tidy_text(&mut a.name, MAX_NAME, "name")?;
    tidy_text(&mut a.breed, MAX_BREED, "breed")?;
    tidy_text(&mut a.notes, MAX_NOTES, "note")?;
    if let Some(b) = a.born
        && b > chrono::Utc::now().date_naive() + chrono::Duration::days(1)
    {
        return Err(ApiError::bad_request("The birth date is in the future."));
    }
    Ok(())
}

/// A tag names one active animal per herd, and an EID one animal on the
/// farm for good: 409 otherwise. Removed animals keep their EID but free
/// their tag. `before` is the stored record on an edit: only a tag, herd or
/// EID that changes is checked, so rows stored before this rule stay editable.
pub async fn check_unique(ctx: &Ctx, a: &Animal, before: Option<&Animal>) -> ApiResult<()> {
    let moved = before.is_none_or(|b| b.tag != a.tag || b.herd_id != a.herd_id);
    if a.removed_at.is_none() && moved {
        let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM animals WHERE herd_id = ? AND tag = ? AND removed_at IS NULL AND id != ? LIMIT 1")
            .bind(&a.herd_id)
            .bind(&a.tag)
            .bind(&a.id)
            .fetch_optional(ctx.db())
            .await?;
        if taken.is_some() {
            return Err(ApiError::conflict(format!("Tag {} is already in this herd.", a.tag)));
        }
    }
    if let Some(eid) = a.eid.as_ref().filter(|e| before.is_none_or(|b| b.eid.as_ref() != Some(*e))) {
        let on: Option<(String,)> =
            sqlx::query_as("SELECT tag FROM animals WHERE eid = ? AND id != ? LIMIT 1").bind(eid).bind(&a.id).fetch_optional(ctx.db()).await?;
        if let Some((tag,)) = on {
            return Err(ApiError::conflict(format!("That EID is on {tag}.")));
        }
    }
    Ok(())
}

/// Whether any animal row (active or removed) belongs to the herd: from then
/// on its count follows its animals.
pub async fn herd_has_animals(ctx: &Ctx, herd_id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM animals WHERE herd_id = ? LIMIT 1").bind(herd_id).fetch_optional(ctx.db()).await?.is_some())
}

/// Set `herds.count` to the herd's active animals, once it has any animal
/// rows. Writes only when the number changes (so history triggers see real
/// changes) and returns the new count then.
pub async fn sync_herd_count<'e>(e: impl SqliteExecutor<'e>, herd_id: &str) -> anyhow::Result<Option<u32>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE herds SET count = (SELECT COUNT(*) FROM animals WHERE herd_id = ?1 AND removed_at IS NULL)
         WHERE id = ?1 AND EXISTS (SELECT 1 FROM animals WHERE herd_id = ?1)
           AND count != (SELECT COUNT(*) FROM animals WHERE herd_id = ?1 AND removed_at IS NULL)
         RETURNING count",
    )
    .bind(herd_id)
    .fetch_optional(e)
    .await?;
    Ok(row.map(|(n,)| n.max(0) as u32))
}

/// [`sync_herd_count`] for a herd animals just left (moved to another herd
/// or deleted): its count is its active animals even when none are left, so
/// the last one leaving sets it to 0 rather than leaving it at 1. With no
/// animal rows left the farmer can set the count again.
pub async fn sync_after_leaving<'e>(e: impl SqliteExecutor<'e>, herd_id: &str) -> anyhow::Result<Option<u32>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE herds SET count = (SELECT COUNT(*) FROM animals WHERE herd_id = ?1 AND removed_at IS NULL)
         WHERE id = ?1 AND count != (SELECT COUNT(*) FROM animals WHERE herd_id = ?1 AND removed_at IS NULL)
         RETURNING count",
    )
    .bind(herd_id)
    .fetch_optional(e)
    .await?;
    Ok(row.map(|(n,)| n.max(0) as u32))
}

/// After an animal of `herd_id` was marked removed as of `at`: the count
/// drops by it now, and a removal dated earlier takes the head off the
/// herd's history from that date too ([`backdate_removal`]), so reports stop
/// counting it then. One write transaction.
///
/// The count still holds every animal marked removed since it was last
/// counted, so it drops by this one only: two removals whose animals were
/// both marked before either count ran each take their own head off from
/// their own date. A count that holds no removed animal is left to
/// [`sync_herd_count`].
pub async fn count_removal(ctx: &Ctx, herd_id: &str, animal_id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut tx = store::begin_immediate(ctx.db()).await?;
    let counts: Option<(i64, i64)> =
        sqlx::query_as("SELECT count, (SELECT COUNT(*) FROM animals WHERE herd_id = ?1 AND removed_at IS NULL) FROM herds WHERE id = ?1")
            .bind(herd_id)
            .fetch_optional(&mut *tx)
            .await?;
    if counts.is_some_and(|(count, active)| count > active) {
        backdate_removal(&mut tx, herd_id, animal_id, at).await?;
        sqlx::query("UPDATE herds SET count = count - 1 WHERE id = ?").bind(herd_id).execute(&mut *tx).await?;
    } else {
        sync_herd_count(&mut *tx, herd_id).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// An animal of `herd_id` left the farm at `at` but is only now taken off
/// the count. Every herd history row from `at` on counted it, so each drops
/// by one, and a row at `at` starts the lower count there (in the paddock the
/// herd was in then): head-days, AU-days and AUM run at the lower count from
/// the day it left, not from the day it was entered. A date at or before the
/// animal's record began means it never counted: the rows from its record on
/// drop, and nothing before them changes. Call before the count itself
/// changes (the trigger's row for that is right as it is). Nothing happens
/// for a time that isn't in the past.
pub async fn backdate_removal(conn: &mut SqliteConnection, herd_id: &str, animal_id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    let created: Option<String> = sqlx::query_scalar("SELECT created_at FROM animals WHERE id = ?").bind(animal_id).fetch_optional(&mut *conn).await?;
    let created = created.as_deref().map(time::from_db).transpose()?;
    let from_record = created.is_some_and(|c| at <= c);
    let at = created.filter(|_| from_record).unwrap_or(at);
    if at >= time::now() {
        return Ok(());
    }
    let at_db = time::to_db(&at);
    let exact: Option<i64> =
        sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? AND at = ? LIMIT 1").bind(herd_id).bind(&at_db).fetch_optional(&mut *conn).await?;
    if !from_record && exact.is_none() {
        // The herd as it stood just before `at`, one head fewer from then.
        let before: Option<(i64, Option<String>, String, String)> =
            sqlx::query_as("SELECT count, paddock_id, name, species FROM herd_history WHERE herd_id = ? AND at < ? ORDER BY at DESC, id DESC LIMIT 1")
                .bind(herd_id)
                .bind(&at_db)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some((count, paddock, name, species)) = before {
            sqlx::query("INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source) VALUES (?, ?, ?, ?, ?, ?, 'changed')")
                .bind(herd_id)
                .bind(&at_db)
                .bind(count)
                .bind(paddock)
                .bind(name)
                .bind(species)
                .execute(&mut *conn)
                .await?;
        }
    }
    sqlx::query("UPDATE herd_history SET count = MAX(count - 1, 0) WHERE herd_id = ? AND at >= ?").bind(herd_id).bind(&at_db).execute(&mut *conn).await?;
    Ok(())
}

// The list_animals tool

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    herd_id: Option<String>,
    q: Option<String>,
    #[serde(default)]
    removed: bool,
}

/// `list_animals` (read): the herd's animals with the collar each wears.
pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "list_animals",
        description: "Animals in a herd (or on the whole farm): tag, name, EID, breed, sex, birth date, and the collar each wears with its battery (0-1), last contact, latest position [lon, lat] and fence state. A parked collar (charging, shelf, repair) says why. Only animals on the farm unless `removed` is true, which lists those sold, died, culled or moved off instead.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "herd_id": { "type": "string", "description": "Herd id. Absent: every herd." },
                "q": { "type": "string", "description": "Only animals whose tag, name or EID contains this text." },
                "removed": { "type": "boolean", "description": "List removed animals instead of those on the farm." }
            },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { list_animals_tool(&c.ctx, c.args).await }),
    }
}

async fn list_animals_tool(ctx: &Ctx, args: Value) -> ApiResult<Value> {
    let args: ListArgs = serde_json::from_value(if args.is_null() { json!({}) } else { args }).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let herds = ctx.store().list_herds().await?;
    let herd_id = args.herd_id.filter(|h| !h.trim().is_empty());
    if let Some(h) = &herd_id
        && !herds.iter().any(|x| &x.id == h)
    {
        return Err(ApiError::bad_request("No such herd."));
    }
    let herd_names: HashMap<&str, &str> = herds.iter().map(|h| (h.id.as_str(), h.name.as_str())).collect();
    let rows = sqlx::query("SELECT * FROM collars").fetch_all(ctx.db()).await?;
    let collars: HashMap<String, Collar> =
        rows.iter().map(store::collar_from_row).collect::<anyhow::Result<Vec<_>>>()?.into_iter().map(|c| (c.id.clone(), c)).collect();
    let q = args.q.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let mut out = Vec::new();
    for a in ctx.store().list_animals(herd_id.as_deref()).await? {
        if a.removed_at.is_some() != args.removed {
            continue;
        }
        if let Some(q) = &q {
            let hay = [Some(&a.tag), a.name.as_ref(), a.eid.as_ref()];
            if !hay.iter().flatten().any(|s| s.to_lowercase().contains(q)) {
                continue;
            }
        }
        let mut v = serde_json::to_value(&a).map_err(anyhow::Error::from)?;
        v["herd"] = json!(herd_names.get(a.herd_id.as_str()).copied().unwrap_or(""));
        if let Some(c) = a.collar_id.as_ref().and_then(|id| collars.get(id)) {
            let mut cv = json!({ "id": c.id, "name": c.name, "state": c.state });
            if let Some(b) = c.battery {
                cv["battery"] = json!((b * 100.0).round() / 100.0);
            }
            if let Some(t) = c.last_seen {
                cv["last_seen"] = json!(t);
            }
            if let Some(f) = &c.last_fix {
                cv["position"] = json!(f.point);
                cv["fix_at"] = json!(f.at);
            }
            if let Some(r) = c.parked_reason {
                cv["parked"] = json!(r);
            }
            v["collar"] = cv;
        }
        out.push(v);
    }
    Ok(json!({ "count": out.len(), "animals": out }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eids_lose_spaces_and_dashes() {
        assert_eq!(normalize_eid("982 000 123 456 789").unwrap(), "982000123456789");
        assert_eq!(normalize_eid("982-000123456789").unwrap(), "982000123456789");
        assert!(normalize_eid("9.82E+14").is_err());
        assert!(normalize_eid("98200012345678").is_err());
        assert!(normalize_eid("98200012345678X").is_err());
    }

    #[test]
    fn birth_dates_in_the_ways_records_write_them() {
        let d = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        assert_eq!(parse_born("2022-04-01", true).unwrap(), d(2022, 4, 1));
        assert_eq!(parse_born("2022-04-01T00:00:00", true).unwrap(), d(2022, 4, 1));
        assert_eq!(parse_born("4/1/2022", true).unwrap(), d(2022, 4, 1));
        assert_eq!(parse_born("4/1/2022", false).unwrap(), d(2022, 1, 4));
        assert_eq!(parse_born("13/4/2022", true).unwrap(), d(2022, 4, 13));
        assert_eq!(parse_born("01.04.2022", true).unwrap(), d(2022, 4, 1));
        assert_eq!(parse_born("2022/04/01", false).unwrap(), d(2022, 4, 1));
        assert_eq!(parse_born("4/1/21", true).unwrap(), d(2021, 4, 1));
        assert_eq!(parse_born("4/1/99", true).unwrap(), d(1999, 4, 1));
        assert!(parse_born("2021", true).is_err());
        assert!(parse_born("2/30/2022", true).is_err());
        assert!(parse_born("spring", true).is_err());
    }

    #[test]
    fn a_file_reads_its_birth_dates_one_way() {
        assert!(!month_first_in(["13/4/2022", "5/4/2022"], true), "13/4 says day first");
        assert!(month_first_in(["4/13/2022", "5/4/2022"], false), "4/13 says month first");
        assert!(month_first_in(["5/4/2022", "2022-04-13", "01.04.2022"], true), "nothing to go on: the zone's");
        assert!(!month_first_in(["5/4/2022"], false));
        assert!(!month_first_in(["13/4/2022", "14/4/2022", "4/15/2022"], true), "the more common");
    }

    #[test]
    fn sex_words() {
        assert_eq!(parse_sex("Heifer"), Some(Sex::Female));
        assert_eq!(parse_sex(" F "), Some(Sex::Female));
        assert_eq!(parse_sex("BULL"), Some(Sex::Male));
        assert_eq!(parse_sex("steer"), Some(Sex::Castrated));
        assert_eq!(parse_sex("x"), None);
    }
}
