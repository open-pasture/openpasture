use op_core::*;
use serde_json::json;

async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

fn square() -> Polygon {
    Polygon::from_ring(vec![[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13]])
}

#[tokio::test]
async fn open_creates_files_and_defaults() {
    let (dir, ctx) = ctx().await;
    assert!(dir.path().join("openpasture.db").exists());
    assert!(dir.path().join(keys::KEY_FILE).exists());
    let s = ctx.settings().await.unwrap();
    assert_eq!(s.server.bind, "127.0.0.1");
    assert_eq!(s.server.port, 7878);
    assert_eq!(s.brain.id, BrainId::Heuristic);
    assert_eq!(s.decision_time, "06:00");
    assert!(s.server.app_token.len() >= 32);
    assert_eq!(ctx.base_url(), "http://127.0.0.1:7878");

    // Reopening keeps the token and the key.
    let token = s.server.app_token.clone();
    let pk = ctx.public_key_b64();
    drop(ctx);
    let again = Ctx::open(dir.path()).await.unwrap();
    assert_eq!(again.settings().await.unwrap().server.app_token, token);
    assert_eq!(again.public_key_b64(), pk);
}

#[tokio::test]
async fn record_crud() {
    let (_dir, ctx) = ctx().await;
    let store = ctx.store();
    assert!(store.get_farm().await.unwrap().is_none());

    let farm = Farm { id: id::new_id(id::FARM), name: "Home".into(), timezone: "America/Chicago".into(), center: [-92.4, 38.12], created_at: time::now() };
    store.insert_farm(&farm).await.unwrap();
    assert_eq!(store.get_farm().await.unwrap().unwrap(), farm);

    let pad = Paddock {
        id: id::new_id(id::PADDOCK),
        name: "North".into(),
        area_ha: square().area_ha(),
        geometry: square(),
        status: PaddockStatus::Resting,
        notes: None,
        grazed_until: None,
        created_at: time::now(),
    };
    store.insert_paddock(&pad).await.unwrap();
    assert_eq!(store.get_paddock(&pad.id).await.unwrap().unwrap(), pad);

    let herd = Herd {
        id: id::new_id(id::HERD),
        name: "Cows".into(),
        species: Species::Cattle,
        count: 30,
        paddock_id: Some(pad.id.clone()),
        autonomy: Autonomy::Propose,
        timer_minutes: 60,
        created_at: time::now(),
    };
    store.insert_herd(&herd).await.unwrap();
    assert_eq!(store.list_herds().await.unwrap(), vec![herd.clone()]);

    let animal = Animal { id: id::new_id(id::ANIMAL), tag: "A1".into(), name: None, herd_id: herd.id.clone(), collar_id: None };
    store.insert_animal(&animal).await.unwrap();
    assert_eq!(store.list_animals(Some(&herd.id)).await.unwrap(), vec![animal.clone()]);

    // Deleting the paddock leaves the herd without one.
    assert!(store.delete_paddock(&pad.id).await.unwrap());
    assert_eq!(store.get_herd(&herd.id).await.unwrap().unwrap().paddock_id, None);

    // Deleting the herd removes its animals.
    assert!(store.delete_herd(&herd.id).await.unwrap());
    assert!(store.get_animal(&animal.id).await.unwrap().is_none());
    assert!(!store.delete_herd(&herd.id).await.unwrap());
}

#[tokio::test]
async fn settings_kv_and_events() {
    let (_dir, ctx) = ctx().await;
    let store = ctx.store();
    store.set_setting("test.thing", &json!({"a": 1})).await.unwrap();
    assert_eq!(store.get_setting_json("test.thing").await.unwrap(), Some(json!({"a": 1})));
    assert_eq!(store.get_setting::<serde_json::Value>("missing").await.unwrap(), None);

    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: "paddock.created".into(),
        source: "farmer".into(),
        occurred_at: time::now(),
        recorded_at: time::now(),
        title: "North paddock drawn".into(),
        body: None,
        payload: json!({"area_ha": 9.7}),
        targets: vec![("paddock".into(), "pad_1".into())],
    };
    store.record_event(&e).await.unwrap();
    assert_eq!(store.list_events(Some(("paddock", "pad_1")), 10).await.unwrap(), vec![e.clone()]);
    assert!(store.list_events(Some(("herd", "x")), 10).await.unwrap().is_empty());
    assert_eq!(store.list_events(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn secrets_file_is_private() {
    let (dir, ctx) = ctx().await;
    let s = ctx.secrets();
    assert_eq!(s.get("openai_api_key").unwrap(), None);
    s.set("openai_api_key", "sk-test").unwrap();
    assert_eq!(s.get("openai_api_key").unwrap().as_deref(), Some("sk-test"));
    assert_eq!(s.list_names().unwrap(), vec!["openai_api_key".to_string()]);
    let status = s.status().unwrap();
    assert!(status.iter().any(|x| x.name == "openai_api_key" && x.set));
    assert!(status.iter().any(|x| x.name == "anthropic_api_key" && !x.set));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(secrets::FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(s.delete("openai_api_key").unwrap());
    assert!(!s.delete("openai_api_key").unwrap());
    assert!(s.set("Bad Name", "x").is_err());
}

#[tokio::test]
async fn events_reach_subscribers() {
    let (_dir, ctx) = ctx().await;
    let mut rx = ctx.subscribe();
    ctx.publish(Event::DecisionLog { decision_id: "dec_1".into(), line: "thinking".into() });
    let got = rx.recv().await.unwrap();
    assert_eq!(serde_json::to_value(&got).unwrap(), json!({"type": "decision_log", "decision_id": "dec_1", "line": "thinking"}));
}

#[tokio::test]
async fn shutdown_signal() {
    let (_dir, ctx) = ctx().await;
    let c = ctx.clone();
    let waiter = tokio::spawn(async move { c.on_shutdown().await });
    ctx.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter).await.unwrap().unwrap();
    assert!(ctx.is_shutting_down());
}

#[test]
fn shapes_match_api_md() {
    let herd = Herd {
        id: "herd_1".into(),
        name: "Cows".into(),
        species: Species::Goats,
        count: 3,
        paddock_id: None,
        autonomy: Autonomy::Timer,
        timer_minutes: 30,
        created_at: time::from_db("2026-09-26T12:00:00.000Z").unwrap(),
    };
    assert_eq!(
        serde_json::to_value(&herd).unwrap(),
        json!({"id": "herd_1", "name": "Cows", "species": "goats", "count": 3, "autonomy": "timer", "timer_minutes": 30, "created_at": "2026-09-26T12:00:00Z"})
    );
    let d: Decision = serde_json::from_value(json!({
        "id": "dec_1", "herd_id": "herd_1", "source": "brain", "brain": "codex", "status": "proposed",
        "action": "NEEDS_INFO", "inputs": {}, "created_at": "2026-09-26T12:00:00Z"
    }))
    .unwrap();
    assert_eq!(d.action, Some(DecisionAction::NeedsInfo));
    assert_eq!(DecisionAction::NeedsInfo.as_db(), "NEEDS_INFO");
    assert_eq!(FenceState::from_db("warning").unwrap(), FenceState::Warning);
    let ev = Event::Ack { collar_id: "col_1".into(), herd_id: "herd_1".into(), version: 4, status: AckStatus::Applied, reason: None };
    assert_eq!(serde_json::to_value(&ev).unwrap(), json!({"type": "ack", "collar_id": "col_1", "herd_id": "herd_1", "version": 4, "status": "applied"}));
}

/// Two settings updates at once both land: the merge happens inside one
/// write transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_settings_updates_keep_both() {
    let (_dir, ctx) = ctx().await;
    for i in 0..10 {
        let (a, b) = (ctx.clone(), ctx.clone());
        let time = format!("0{}:1{}", i % 10, i % 10);
        let units = if i % 2 == 0 { "imperial" } else { "metric" };
        let (ra, rb) = tokio::join!(async move { a.update_settings(&json!({ "decision_time": time })).await }, async move {
            b.update_settings(&json!({ "units": units })).await
        },);
        ra.unwrap();
        rb.unwrap();
        let s = ctx.settings().await.unwrap();
        assert_eq!(s.decision_time, format!("0{}:1{}", i % 10, i % 10));
        assert_eq!(serde_json::to_value(&s.units).unwrap(), json!(units));
    }
    // Brain tokens: valid until dropped.
    let t = ctx.mint_brain_token(std::time::Duration::from_secs(60));
    let s = t.as_str().to_owned();
    assert!(ctx.check_brain_token(&s));
    assert!(!ctx.check_brain_token("opb_nope"));
    drop(t);
    assert!(!ctx.check_brain_token(&s));
    let t = ctx.mint_brain_token(std::time::Duration::from_millis(1));
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(!ctx.check_brain_token(t.as_str()), "expired");
}
