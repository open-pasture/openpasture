//! Numbers people see or receive. The API is SI (m, ha, kg, cm, m²); every
//! report, text and brief formats through [`Fmt`], driven by the farm's
//! `settings.units`. The UI mirrors this in `ui/src/units.ts`.
//!
//! Texts are GSM-7: `²` is not, so texts don't use [`Fmt::per_head`].

use crate::Ctx;
pub use crate::domain::Units;

const AC_PER_HA: f64 = 2.471_053_814_671_653;
const FT_PER_M: f64 = 3.280_839_895_013_123;
const CM_PER_IN: f64 = 2.54;
const LB_PER_KG: f64 = 2.204_622_621_848_776;
const FT2_PER_M2: f64 = 10.763_910_416_709_722;

/// IANA zones of the United States and its territories.
const US_ZONES: &[&str] = &[
    "America/New_York",
    "America/Detroit",
    "America/Kentucky/Louisville",
    "America/Kentucky/Monticello",
    "America/Louisville",
    "America/Indiana/Indianapolis",
    "America/Indiana/Vincennes",
    "America/Indiana/Winamac",
    "America/Indiana/Marengo",
    "America/Indiana/Petersburg",
    "America/Indiana/Vevay",
    "America/Indiana/Tell_City",
    "America/Indiana/Knox",
    "America/Indianapolis",
    "America/Fort_Wayne",
    "America/Knox_IN",
    "America/Chicago",
    "America/Menominee",
    "America/North_Dakota/Center",
    "America/North_Dakota/New_Salem",
    "America/North_Dakota/Beulah",
    "America/Denver",
    "America/Boise",
    "America/Phoenix",
    "America/Los_Angeles",
    "America/Anchorage",
    "America/Juneau",
    "America/Sitka",
    "America/Metlakatla",
    "America/Yakutat",
    "America/Nome",
    "America/Adak",
    "America/Atka",
    "America/Puerto_Rico",
    "America/St_Thomas",
    "America/Virgin",
    "Pacific/Honolulu",
    "Pacific/Johnston",
    "Pacific/Guam",
    "Pacific/Saipan",
    "Pacific/Pago_Pago",
    "Pacific/Samoa",
    "Pacific/Midway",
    "Pacific/Wake",
    "Navajo",
];

/// Imperial for US zones (America/Chicago, America/New_York, …,
/// Pacific/Honolulu, America/Puerto_Rico and the `US/…` aliases), else metric.
pub fn units_for_timezone(tz: &str) -> Units {
    let tz = tz.trim();
    if tz.starts_with("US/") || US_ZONES.contains(&tz) { Units::Imperial } else { Units::Metric }
}

/// Formats SI values in the farm's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fmt {
    pub units: Units,
}

impl Fmt {
    pub fn new(units: Units) -> Self {
        Self { units }
    }

    /// From the farm's settings.
    pub async fn of(ctx: &Ctx) -> anyhow::Result<Self> {
        Ok(Self { units: ctx.settings().await?.units })
    }

    fn imperial(&self) -> bool {
        self.units == Units::Imperial
    }

    /// "30.6 ac" | "12.4 ha": one decimal, two below 0.1, none from 1,000.
    pub fn area(&self, ha: f64) -> String {
        let v = self.convert("area", ha);
        let a = v.abs();
        let decimals = if a >= 1000.0 {
            0
        } else if a > 0.0 && a < 0.1 {
            2
        } else {
            1
        };
        format!("{} {}", num(v, decimals), self.unit_label("area"))
    }

    /// "200 ft" | "60 m": whole units below 100, the nearest 10 from there.
    pub fn len(&self, m: f64) -> String {
        let v = self.convert("len", m);
        let v = if v.abs() < 100.0 { v } else { (v / 10.0).round() * 10.0 };
        format!("{} {}", num(v, 0), self.unit_label("len"))
    }

    /// "4 in" | "10 cm": whole units.
    pub fn height(&self, cm: f64) -> String {
        format!("{} {}", num(self.convert("height", cm), 0), self.unit_label("height"))
    }

    /// "1,200 lb" | "545 kg": three significant figures, at least whole numbers.
    pub fn mass(&self, kg: f64) -> String {
        format!("{} {}", sig3(self.convert("mass", kg)), self.unit_label("mass"))
    }

    /// "5,340 ft²/hd" | "496 m²/hd": three significant figures, at least whole numbers.
    pub fn per_head(&self, m2: f64) -> String {
        format!("{} {}", sig3(self.convert("per_head", m2)), self.unit_label("per_head"))
    }

    /// "8.2 AU/ac" | "20.2 AU/ha": one decimal.
    pub fn density(&self, au: f64, ha: f64) -> String {
        let per = if ha > 0.0 { self.convert("density", au / ha) } else { 0.0 };
        format!("{} {}", num(per, 1), self.unit_label("density"))
    }

    /// Unit of a report column: `area`, `len`, `height`, `mass`, `per_head`,
    /// `density`; "" for anything else (counts, days).
    pub fn unit_label(&self, quantity: &str) -> &'static str {
        let imp = self.imperial();
        match quantity {
            "area" => pick(imp, "ac", "ha"),
            "len" | "length" => pick(imp, "ft", "m"),
            "height" => pick(imp, "in", "cm"),
            "mass" => pick(imp, "lb", "kg"),
            "per_head" => pick(imp, "ft²/hd", "m²/hd"),
            "density" => pick(imp, "AU/ac", "AU/ha"),
            _ => "",
        }
    }

    /// An SI value in the unit [`Fmt::unit_label`] names, for report cells.
    /// `density` takes AU per hectare. Unknown quantities pass through.
    pub fn convert(&self, quantity: &str, si: f64) -> f64 {
        if !self.imperial() {
            return si;
        }
        match quantity {
            "area" => si * AC_PER_HA,
            "len" | "length" => si * FT_PER_M,
            "height" => si / CM_PER_IN,
            "mass" => si * LB_PER_KG,
            "per_head" => si * FT2_PER_M2,
            "density" => si / AC_PER_HA,
            _ => si,
        }
    }
}

fn pick(imperial: bool, a: &'static str, b: &'static str) -> &'static str {
    if imperial { a } else { b }
}

/// `decimals` places, "," between thousands (the same in every locale, as
/// `ui/src/units.ts` prints): 5340 → "5,340".
fn num(v: f64, decimals: usize) -> String {
    let scale = 10f64.powi(decimals as i32);
    let n = (v.abs() * scale).round();
    let int = (n / scale).trunc() as u64;
    let digits = int.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + decimals + 2);
    if v < 0.0 && n > 0.0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if decimals > 0 {
        let frac = (n - int as f64 * scale).round() as u64;
        out.push('.');
        out.push_str(&format!("{frac:0decimals$}"));
    }
    out
}

/// Three significant figures, at least whole numbers: 5338.9 → "5,340", 496.2 → "496".
fn sig3(v: f64) -> String {
    let a = v.abs();
    if a < 1000.0 {
        return num(v, 0);
    }
    let step = 10f64.powi(a.log10().floor() as i32 - 2);
    num((v / step).round() * step, 0)
}
