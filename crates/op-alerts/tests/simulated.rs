//! Alerts about the simulator's collars (`collars.simulated`): shown in the
//! app and sent to the farm webhook, marked `data.simulated`, and texted to
//! nobody. A real collar's alert in the same herd still goes out.

mod a_engine_fixture;

use a_engine_fixture::*;
use op_core::Role;
use serde_json::json;

async fn person_and_hook(f: &Farm) -> String {
    f.channels(json!({"sms": {"from": "+15155550100"}, "webhook": {"url": "http://127.0.0.1:9/hook"}})).await;
    f.person("Cody", Role::Owner, Some("+15155550101"), true, None).await
}

#[tokio::test]
async fn a_simulated_collar_outside_is_shown_and_texted_to_nobody() {
    let f = Farm::new().await;
    let cody = person_and_hook(&f).await;
    let t = t0();
    let c = f.collar(Some("101"), t).await;
    f.set(&c, "simulated = 1", &[]).await;
    f.outside(&c, t - mins(5), t).await;
    f.eval(t).await;
    let open = f.open_kind("outside").await;
    assert_eq!(open.len(), 1, "the app still shows it");
    assert_eq!(open[0].data["simulated"], json!(true));

    let mut sent = Vec::new();
    for s in [0, 30, 60, 120, 600] {
        sent.extend(f.route(t + secs(s)).await);
    }
    assert!(f.messages_to(&cody).await.is_empty(), "nobody is texted about the simulator");
    assert!(sent.iter().all(|m| m.channel == "webhook"), "{sent:#?}");
    assert_eq!(sent.len(), 1, "the farm webhook gets it once, marked");
}

#[tokio::test]
async fn a_real_collar_beside_a_simulated_one_is_still_texted() {
    let f = Farm::new().await;
    let cody = person_and_hook(&f).await;
    let t = t0();
    let real = f.collar(Some("101"), t).await;
    let sim = f.collar(Some("901"), t).await;
    f.set(&sim, "simulated = 1", &[]).await;
    for c in [&real, &sim] {
        f.outside(c, t - mins(5), t).await;
    }
    f.eval(t).await;
    let open = f.open_kind("outside").await;
    assert_eq!(open.len(), 2);
    let marked: Vec<bool> = open.iter().map(|a| a.data["simulated"] == json!(true)).collect();
    assert_eq!(marked.iter().filter(|m| **m).count(), 1, "only the simulated collar's alert is marked");
    for s in [0, 30, 60, 120, 600] {
        f.route(t + secs(s)).await;
    }
    let texts = f.messages_to(&cody).await;
    assert_eq!(texts.len(), 1, "{texts:#?}");
    assert!(texts[0].text.contains("101") && !texts[0].text.contains("901"), "{}", texts[0].text);
}
