//! The UI and the texts print the same numbers: `ui/src/units.ts` mirrors
//! `op_core::units`. `ui/src/units.vectors.json` holds op_core's answers for a
//! spread of inputs (rounding halves, the 0.1 / 100 / 1,000 edges, negatives,
//! nine orders of magnitude); this test keeps the file equal to what op_core
//! prints now and `ui/src/units.test.ts` checks units.ts against the same file.
//!
//! After changing `op_core::units`: `OP_WRITE_UNITS_VECTORS=1 cargo test -p
//! op-core --test units_vectors`, then `bun test` in `ui/` and fix units.ts.

use std::path::PathBuf;

use op_core::units::{Fmt, Units};

fn inputs() -> Vec<f64> {
    let mut v = vec![
        0.0, 0.004, 0.005, 0.0049, 0.0149, 0.015, 0.04, 0.045, 0.05, 0.085, 0.095, 0.0995, 0.09999, 0.1, 0.15, 0.25, 0.285, 0.35, 0.45, 1.005, 1.214, 1.25,
        2.5, 4.35, 4.45, 7.5, 9.95, 12.4, 16.5, 30.48, 30.6, 41.0, 60.0, 99.4, 99.5, 99.95, 100.0, 104.9, 105.0, 115.0, 402.0, 404.7, 496.0, 496.2, 545.0,
        999.4, 999.5, 999.95, 1000.0, 1004.9, 1005.0, 1234.5, 1504.0, 5338.9, 9995.0, 99950.0, 1234567.89, -0.04, -0.045, -1.25, -4.35, -12.0, -60.0, -99.5,
        -105.0, -1005.0, -1200.5,
    ];
    v.extend((1..=200).map(|i| f64::from(i) / 20.0)); // every halfway point between tenths up to 10
    v.extend((1..=40).map(|i| f64::from(i) * 0.0025)); // hundredths under 0.1
    v.extend((0..=40).map(|i| 28.0 + f64::from(i) * 0.125)); // around 100 ft (30.48 m)
    v.extend((0..=40).map(|i| 95.0 + f64::from(i) * 0.25)); // around 100 m
    // Nine orders of magnitude from a fixed-seed xorshift, every fifth negative.
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 0..120 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let u = (s >> 11) as f64 / (1u64 << 53) as f64;
        let x = 10f64.powf(u * 9.0 - 3.0);
        v.push(if i % 5 == 0 { -x } else { x });
    }
    v
}

fn density_inputs() -> Vec<(f64, f64)> {
    let mut v = Vec::new();
    for au in [0.0, 1.0, 12.0, 37.5, 250.0, 251.0, 1000.0] {
        for ha in [-1.0, 0.0, 0.5, 1.214, 12.4, 16.5, 33.3, 100.7, 404.7] {
            v.push((au, ha));
        }
    }
    v
}

/// One array per line, so a change shows up as a readable diff.
fn render() -> String {
    let si = inputs();
    let pairs = density_inputs();
    let mut out = format!("{{\n  \"si\": {},\n  \"density_in\": {},\n", json(&si), json(&pairs));
    for (name, units) in [("metric", Units::Metric), ("imperial", Units::Imperial)] {
        let f = Fmt::new(units);
        let each = |g: &dyn Fn(f64) -> String| si.iter().map(|x| g(*x)).collect::<Vec<_>>();
        out += &format!("  \"{name}\": {{\n");
        out += &format!("    \"area\": {},\n", json(&each(&|x| f.area(x))));
        out += &format!("    \"len\": {},\n", json(&each(&|x| f.len(x))));
        out += &format!("    \"height\": {},\n", json(&each(&|x| f.height(x))));
        out += &format!("    \"mass\": {},\n", json(&each(&|x| f.mass(x))));
        out += &format!("    \"per_head\": {},\n", json(&each(&|x| f.per_head(x))));
        out += &format!("    \"density\": {}\n", json(&pairs.iter().map(|(au, ha)| f.density(*au, *ha)).collect::<Vec<_>>()));
        out += if units == Units::Metric { "  },\n" } else { "  }\n" };
    }
    out += "}\n";
    out
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("vectors serialize")
}

#[test]
fn ui_units_vectors_are_what_op_core_units_prints() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/units.vectors.json");
    let want = render();
    if std::env::var_os("OP_WRITE_UNITS_VECTORS").is_some() {
        std::fs::write(&path, &want).expect("write vectors");
    }
    let have = std::fs::read_to_string(&path).expect("ui/src/units.vectors.json");
    if let Some((n, (a, b))) = have.lines().zip(want.lines()).enumerate().find(|(_, (a, b))| a != b) {
        let at = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
        let near = |s: &str| s.chars().skip(at.saturating_sub(40)).take(120).collect::<String>();
        panic!(
            "ui/src/units.vectors.json line {} differs from op_core::units:\n  file: …{}…\n  now:  …{}…\nRerun with OP_WRITE_UNITS_VECTORS=1, then `bun test` in ui/ and bring units.ts along.",
            n + 1,
            near(a),
            near(b)
        );
    }
    assert_eq!(have.lines().count(), want.lines().count(), "ui/src/units.vectors.json has a different number of lines; rerun with OP_WRITE_UNITS_VECTORS=1");
}
