use super::scheduler::{binding_key, window_key};
use super::*;
use crate::model::{Rejection, Window};
use chrono::TimeZone;
use std::collections::HashSet;

const MODEL: &str = "gpt-5.3-codex";
fn pool(ids: &[&str]) -> Vec<Account> {
    ids.iter()
        .map(|id| Account {
            id: (*id).into(),
            provider: "codex".into(),
            enabled: true,
            ..Account::default()
        })
        .collect()
}
fn scheduler(ids: &[&str]) -> (Scheduler, Arc<Mutex<DateTime<Utc>>>) {
    let mut s = Scheduler::new("", pool(ids)).unwrap();
    let clock = Arc::new(Mutex::new(
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap(),
    ));
    let c = clock.clone();
    s.now = Arc::new(move || *c.lock().unwrap());
    (s, clock)
}
fn now(c: &Arc<Mutex<DateTime<Utc>>>) -> DateTime<Utc> {
    *c.lock().unwrap()
}
fn advance(c: &Arc<Mutex<DateTime<Utc>>>, d: Duration) {
    *c.lock().unwrap() += d;
}
fn window(name: &str, used: f64, reset: Option<DateTime<Utc>>, minutes: i64) -> Window {
    Window {
        name: name.into(),
        used: Some(used),
        resets_at: reset,
        duration: minutes * NANOS_PER_MINUTE,
        ..Window::default()
    }
}
fn update(s: &Scheduler, id: &str, at: DateTime<Utc>, windows: Vec<Window>) {
    s.update(Snapshot {
        account_id: id.into(),
        observed_at: at,
        available: true,
        windows,
        ..Snapshot::default()
    })
    .unwrap();
}
fn acquire(s: &Scheduler, session: &str) -> Lease {
    s.acquire("codex", session, MODEL, &HashSet::new()).unwrap()
}
fn done(s: &Scheduler, l: &Lease) {
    s.accept(l).unwrap();
    s.complete(l).unwrap();
}
fn unavailable(s: &Scheduler, model: &str) -> Option<DateTime<Utc>> {
    let err = s.acquire("codex", "", model, &HashSet::new()).unwrap_err();
    err.downcast_ref::<UnavailableError>().unwrap().retry_at
}

#[test]
fn affinity_expiry_failover_and_no_snapback() {
    let (s, c) = scheduler(&["a", "b"]);
    let t = now(&c);
    update(
        &s,
        "a",
        t,
        vec![window("week", 95.0, Some(t + Duration::minutes(10)), 10080)],
    );
    update(
        &s,
        "b",
        t,
        vec![window("week", 20.0, Some(t + Duration::days(5)), 10080)],
    );
    let l = acquire(&s, "conversation");
    assert_eq!(l.account.id, "a");
    done(&s, &l);
    advance(&c, Duration::seconds(1));
    let t = now(&c);
    update(
        &s,
        "b",
        t,
        vec![window("week", 0.0, Some(t + Duration::minutes(1)), 10080)],
    );
    let l = acquire(&s, "conversation");
    assert_eq!(l.account.id, "a");
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            window: "week".into(),
            until: Some(t + Duration::minutes(10)),
            ..Rejection::default()
        },
    )
    .unwrap();
    let l = acquire(&s, "conversation");
    assert_eq!(l.account.id, "b");
    assert_eq!(
        s.guard().state.bindings[&binding_key("codex", "conversation")],
        "a"
    );
    done(&s, &l);
    advance(&c, Duration::minutes(11));
    let t = now(&c);
    update(
        &s,
        "a",
        t,
        vec![window("week", 0.0, Some(t + Duration::minutes(1)), 10080)],
    );
    update(
        &s,
        "b",
        t,
        vec![window("week", 80.0, Some(t + Duration::days(7)), 10080)],
    );
    assert_eq!(acquire(&s, "conversation").account.id, "b");
}
#[test]
fn transient_and_throughput_preserve_original_affinity() {
    for kind in ["transient", "throughput"] {
        let (s, c) = scheduler(&["a", "b"]);
        let t = now(&c);
        for id in ["a", "b"] {
            update(
                &s,
                id,
                t,
                vec![window("short", 20.0, Some(t + Duration::hours(1)), 300)],
            );
        }
        done(&s, &acquire(&s, "session"));
        let l = acquire(&s, "session");
        s.reject(
            &l,
            Rejection {
                kind: kind.into(),
                ..Rejection::default()
            },
        )
        .unwrap();
        let l = acquire(&s, "session");
        assert_eq!(l.account.id, "b");
        done(&s, &l);
        assert_eq!(
            s.guard().state.bindings[&binding_key("codex", "session")],
            "a"
        );
        advance(&c, Duration::seconds(3));
        assert_eq!(acquire(&s, "session").account.id, "a");
    }
}
#[test]
fn delayed_accept_cannot_undo_concurrent_failover() {
    let (s, c) = scheduler(&["a", "b"]);
    let t = now(&c);
    for id in ["a", "b"] {
        update(
            &s,
            id,
            t,
            vec![window("short", 20.0, Some(t + Duration::hours(1)), 300)],
        );
    }
    done(&s, &acquire(&s, "session"));
    let delayed = acquire(&s, "session");
    let rejected = acquire(&s, "session");
    s.reject(
        &rejected,
        Rejection {
            kind: "quota".into(),
            until: Some(t + Duration::hours(1)),
            ..Rejection::default()
        },
    )
    .unwrap();
    done(&s, &acquire(&s, "session"));
    done(&s, &delayed);
    assert_eq!(
        s.guard().state.bindings[&binding_key("codex", "session")],
        "b"
    );
}
#[test]
fn concurrent_provisional_affinity_and_atomic_reservations() {
    for same_session in [true, false] {
        let (s, c) = scheduler(&["a", "b", "c"]);
        let t = now(&c);
        for id in ["a", "b", "c"] {
            update(
                &s,
                id,
                t,
                vec![window("week", 50.0, Some(t + Duration::days(7)), 10080)],
            );
        }
        let s = Arc::new(s);
        let threads: Vec<_> = (0..20)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || acquire(&s, if same_session { "new" } else { "" }))
            })
            .collect();
        let leases: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(s.guard().state.bindings.is_empty());
        let mut counts = HashMap::new();
        let mut ids = HashSet::new();
        for l in &leases {
            *counts.entry(l.account.id.clone()).or_insert(0) += 1;
            assert!(ids.insert(l.id.clone()));
        }
        if same_session {
            assert_eq!(counts.len(), 1);
        } else {
            assert_eq!(counts.len(), 3);
            assert!(counts.values().all(|n| (6..=7).contains(n)));
        }
        for l in &leases {
            done(&s, l);
            s.complete(l).unwrap();
        }
        let state = s.guard();
        assert!(state.leases.is_empty());
        assert_eq!(state.state.completed.values().sum::<u64>(), 20);
        assert_eq!(state.state.bindings.len(), usize::from(same_session));
    }
}
#[test]
fn unknown_fallback_bounded_without_displacing_affinity() {
    let (s, c) = scheduler(&["unknown", "known"]);
    let t = now(&c);
    update(
        &s,
        "known",
        t,
        vec![window("short", 99.99, Some(t + Duration::hours(1)), 300)],
    );
    let l = acquire(&s, "");
    assert_eq!(l.account.id, "known");
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            until: Some(t + Duration::hours(1)),
            ..Rejection::default()
        },
    )
    .unwrap();
    let first = acquire(&s, "");
    let second = acquire(&s, "");
    assert!(s.acquire("codex", "", MODEL, &HashSet::new()).is_err());
    s.complete(&first).unwrap();
    s.complete(&second).unwrap();
    let (s, _) = scheduler(&["a", "b"]);
    let first = acquire(&s, "same");
    for _ in 0..5 {
        assert_eq!(acquire(&s, "same").account.id, first.account.id);
    }
    assert_ne!(acquire(&s, "other").account.id, first.account.id);
}
#[test]
fn all_windows_and_auth_blockers_determine_retry() {
    let (s, c) = scheduler(&["a", "b", "c"]);
    let t = now(&c);
    update(
        &s,
        "a",
        t,
        vec![
            window("short", 100.0, Some(t + Duration::minutes(5)), 300),
            window("week", 100.0, Some(t + Duration::hours(1)), 10080),
        ],
    );
    update(
        &s,
        "b",
        t,
        vec![window("short", 100.0, Some(t + Duration::minutes(30)), 300)],
    );
    s.update(Snapshot {
        account_id: "b".into(),
        observed_at: t + Duration::nanoseconds(1),
        auth_invalid: true,
        ..Snapshot::default()
    })
    .unwrap();
    update(
        &s,
        "c",
        t,
        vec![window("short", 100.0, Some(t + Duration::minutes(40)), 300)],
    );
    assert_eq!(unavailable(&s, MODEL), Some(t + Duration::minutes(40)));
    advance(&c, Duration::minutes(6));
    assert_eq!(unavailable(&s, MODEL), Some(t + Duration::minutes(40)));
}
#[test]
fn scoped_windows_match_complete_family_and_exact_codex_model() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    let mut w = window(
        "seven_day_sonnet",
        100.0,
        Some(t + Duration::hours(1)),
        10080,
    );
    w.model = "sonnet".into();
    update(
        &s,
        "a",
        t,
        vec![
            window("five_hour", 50.0, Some(t + Duration::hours(1)), 300),
            w.clone(),
        ],
    );
    unavailable(&s, "claude-sonnet-4-6");
    assert!(
        s.acquire("codex", "", "claude-opus-4-6", &HashSet::new())
            .is_ok()
    );
    advance(&c, Duration::seconds(1));
    w.model = "GPT-5.3-Codex".into();
    update(&s, "a", now(&c), vec![w]);
    unavailable(&s, MODEL);
    assert!(
        s.acquire("codex", "", "gpt-5.3-codex-spark", &HashSet::new())
            .is_ok()
    );
    assert!(s.acquire("claude", "", "allowed", &HashSet::new()).is_err());
}
#[test]
fn backend_rejection_needs_new_matching_positive_authority() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    update(
        &s,
        "a",
        t - Duration::minutes(1),
        vec![window("short", 10.0, Some(t + Duration::hours(1)), 300)],
    );
    let l = acquire(&s, "session");
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            window: "short".into(),
            until: Some(t + Duration::hours(1)),
            ..Rejection::default()
        },
    )
    .unwrap();
    update(
        &s,
        "a",
        t - Duration::seconds(1),
        vec![window("short", 11.0, Some(t + Duration::hours(1)), 300)],
    );
    unavailable(&s, MODEL);
    for w in [
        Window {
            name: "short".into(),
            duration: 300 * NANOS_PER_MINUTE,
            ..Window::default()
        },
        window("different", 5.0, Some(t + Duration::hours(1)), 300),
        window("short", 12.0, Some(t + Duration::hours(1)), 300),
    ] {
        advance(&c, Duration::seconds(1));
        update(&s, "a", now(&c), vec![w]);
        unavailable(&s, MODEL);
    }
    advance(&c, Duration::seconds(1));
    let mut w = window("short", 12.0, Some(t + Duration::hours(1)), 300);
    w.allowed = Some(true);
    update(&s, "a", now(&c), vec![w]);
    assert!(
        s.acquire("codex", "session", MODEL, &HashSet::new())
            .is_ok()
    );
}
#[test]
fn unknown_backend_reset_is_bounded_and_healthy_week_is_not_used() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    update(
        &s,
        "a",
        t,
        vec![
            window("short", 95.0, Some(t + Duration::hours(1)), 300),
            window("week", 20.0, Some(t + Duration::days(7)), 10080),
        ],
    );
    let l = acquire(&s, "");
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            ..Rejection::default()
        },
    )
    .unwrap();
    assert_eq!(unavailable(&s, MODEL), Some(t + Duration::minutes(1)));
    advance(&c, Duration::minutes(1));
    let l = acquire(&s, "");
    advance(&c, Duration::seconds(1));
    update(
        &s,
        "a",
        now(&c),
        vec![
            window("short", 100.0, Some(t + Duration::hours(1)), 300),
            window("week", 20.0, Some(t + Duration::days(7)), 10080),
        ],
    );
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            ..Rejection::default()
        },
    )
    .unwrap();
    assert_eq!(unavailable(&s, MODEL), Some(t + Duration::hours(1)));
}
#[test]
fn partial_and_unavailable_telemetry_preserve_independent_exhaustion() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    update(
        &s,
        "a",
        t,
        vec![
            window("short", 10.0, Some(t + Duration::hours(1)), 300),
            window("week", 100.0, Some(t + Duration::days(1)), 10080),
        ],
    );
    advance(&c, Duration::seconds(1));
    update(
        &s,
        "a",
        now(&c),
        vec![window("short", 11.0, Some(t + Duration::hours(1)), 300)],
    );
    unavailable(&s, MODEL);
    advance(&c, Duration::seconds(1));
    s.update(Snapshot {
        account_id: "a".into(),
        observed_at: now(&c),
        ..Snapshot::default()
    })
    .unwrap();
    unavailable(&s, MODEL);
    advance(&c, Duration::seconds(1));
    update(
        &s,
        "a",
        now(&c),
        vec![window("week", 50.0, Some(t + Duration::days(1)), 10080)],
    );
    assert!(s.acquire("codex", "", MODEL, &HashSet::new()).is_ok());
}
#[test]
fn allowed_overrides_rounding_but_not_explicit_exhaustion() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    let mut w = window("short", 100.0, Some(t + Duration::hours(1)), 300);
    w.allowed = Some(true);
    update(&s, "a", t, vec![w.clone()]);
    s.complete(&acquire(&s, "")).unwrap();
    advance(&c, Duration::seconds(1));
    w.allowed = Some(false);
    w.used = Some(10.0);
    update(&s, "a", now(&c), vec![w.clone()]);
    unavailable(&s, MODEL);
    advance(&c, Duration::seconds(1));
    w.allowed = Some(true);
    w.used = Some(100.0);
    update(&s, "a", now(&c), vec![w.clone()]);
    s.complete(&acquire(&s, "")).unwrap();
    advance(&c, Duration::seconds(1));
    w.exhausted = true;
    update(&s, "a", now(&c), vec![w]);
    unavailable(&s, MODEL);
}
#[test]
fn first_use_predictions_require_explicit_dormancy_and_acceptance() {
    for dormant in [false, true] {
        let (s, c) = scheduler(&["a"]);
        let t = now(&c);
        let mut w = window("short", 0.0, None, 300);
        w.dormant = dormant;
        update(&s, "a", t, vec![w.clone()]);
        let l = acquire(&s, "session");
        assert!(s.guard().state.clocks["a"].is_empty());
        s.complete(&l).unwrap();
        let l = acquire(&s, "session");
        s.accept(&l).unwrap();
        let clocks = s.guard().state.clocks["a"].clone();
        assert_eq!(
            clocks.get(&window_key(&w)).copied(),
            dormant.then_some(t + Duration::hours(5))
        );
        s.complete(&l).unwrap();
        advance(&c, Duration::minutes(1));
        w.resets_at = Some(now(&c) + Duration::hours(2));
        update(&s, "a", now(&c), vec![w.clone()]);
        assert!(!s.guard().state.clocks["a"].contains_key(&window_key(&w)));
    }
}
#[test]
fn per_window_learning_reservations_and_new_epoch() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    let mut short = window("short", 10.0, Some(t + Duration::hours(5)), 300);
    let mut week = window("week", 10.0, Some(t + Duration::days(7)), 10080);
    update(&s, "a", t, vec![short.clone(), week.clone()]);
    done(&s, &acquire(&s, ""));
    done(&s, &acquire(&s, ""));
    advance(&c, Duration::seconds(1));
    short.used = Some(14.0);
    week.used = Some(10.2);
    update(&s, "a", now(&c), vec![short.clone(), week.clone()]);
    assert_eq!(s.guard().state.learning["a"][&window_key(&short)].mean, 2.0);
    assert!((s.guard().state.learning["a"][&window_key(&week)].mean - 0.1).abs() < 1e-9);
    let l = acquire(&s, "");
    let reserved = s.guard().window_work("a", &short);
    done(&s, &l);
    assert_eq!(s.guard().window_work("a", &short), reserved);
    advance(&c, Duration::seconds(1));
    short.used = Some(16.0);
    update(&s, "a", now(&c), vec![short.clone()]);
    assert_eq!(s.guard().window_work("a", &short), 42.0);
    done(&s, &acquire(&s, ""));
    advance(&c, Duration::hours(6));
    short.used = Some(30.0);
    short.resets_at = Some(now(&c) + Duration::hours(5));
    let samples = s.guard().state.learning["a"][&window_key(&short)].samples;
    update(&s, "a", now(&c), vec![short.clone()]);
    assert_eq!(
        s.guard().state.learning["a"][&window_key(&short)].samples,
        samples
    );
}
#[test]
fn scoped_accounting_and_scoped_backend_recovery() {
    let (s, c) = scheduler(&["a"]);
    let t = now(&c);
    let mut w = window("seven_day_sonnet", 20.0, Some(t + Duration::days(1)), 10080);
    w.model = "sonnet".into();
    update(&s, "a", t, vec![w.clone()]);
    let before = s.guard().window_work("a", &w);
    let l = s
        .acquire("codex", "", "claude-opus-4-6", &HashSet::new())
        .unwrap();
    done(&s, &l);
    assert_eq!(s.guard().window_work("a", &w), before);
    let l = s
        .acquire("codex", "", "claude-sonnet-4-6", &HashSet::new())
        .unwrap();
    assert!(s.guard().window_work("a", &w) < before);
    done(&s, &l);
    advance(&c, Duration::seconds(1));
    w.used = Some(22.0);
    update(&s, "a", now(&c), vec![w.clone()]);
    assert_eq!(s.guard().state.learning["a"][&window_key(&w)].mean, 2.0);
    let l = s
        .acquire("codex", "", "claude-sonnet-4-6", &HashSet::new())
        .unwrap();
    s.reject(
        &l,
        Rejection {
            kind: "quota".into(),
            model: "claude-sonnet-4-6".into(),
            until: Some(t + Duration::hours(1)),
            ..Rejection::default()
        },
    )
    .unwrap();
    advance(&c, Duration::seconds(1));
    let mut global = window("short", 20.0, Some(t + Duration::hours(1)), 300);
    global.allowed = Some(true);
    update(&s, "a", now(&c), vec![global]);
    unavailable(&s, "claude-sonnet-4-6");
    advance(&c, Duration::seconds(1));
    w.allowed = Some(true);
    update(&s, "a", now(&c), vec![w]);
    assert!(
        s.acquire("codex", "", "claude-sonnet-4-6", &HashSet::new())
            .is_ok()
    );
}
#[test]
fn persistent_metadata_private_status_and_read_only_reauth() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("private/scheduler.json");
    let mut accounts = pool(&["a", "b"]);
    accounts[0].profile = "secret-profile".into();
    accounts[0].endpoint = "https://secret-token@example.invalid".into();
    let s = Scheduler::new(&path, accounts.clone()).unwrap();
    let t = Utc::now();
    let mut w = window("short", 0.0, None, 300);
    w.dormant = true;
    update(&s, "a", t, vec![w]);
    done(&s, &acquire(&s, "private-session"));
    let l = acquire(&s, "private-session");
    s.reject(
        &l,
        Rejection {
            kind: "auth".into(),
            ..Rejection::default()
        },
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    accounts[0].last_login = Some(Utc::now() + Duration::seconds(1));
    let status = Scheduler::read_status(&path, accounts.clone()).unwrap();
    assert_eq!(before, std::fs::read(&path).unwrap());
    let serialized = serde_json::to_string(&status).unwrap();
    for secret in ["private-session", "secret-profile", "secret-token"] {
        assert!(!serialized.contains(secret));
        assert!(!String::from_utf8_lossy(&before).contains(secret));
    }
    let restored = Scheduler::new(&path, accounts).unwrap();
    assert_eq!(restored.guard().state.bindings.len(), 1);
    assert_eq!(restored.guard().state.clocks["a"].len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
#[test]
fn reauthentication_only_clears_older_auth_blockers() {
    let (s, c) = scheduler(&["a"]);
    let l = acquire(&s, "session");
    let other = acquire(&s, "session");
    s.reject(
        &l,
        Rejection {
            kind: "auth".into(),
            ..Rejection::default()
        },
    )
    .unwrap();
    s.reject(
        &other,
        Rejection {
            kind: "quota".into(),
            window: "week".into(),
            until: Some(now(&c) + Duration::hours(1)),
            ..Rejection::default()
        },
    )
    .unwrap();
    advance(&c, Duration::seconds(1));
    let mut a = pool(&["a"]);
    a[0].last_login = Some(now(&c));
    s.set_accounts(a).unwrap();
    let state = s.guard();
    assert_eq!(state.state.blockers["a"].len(), 1);
    assert_eq!(state.state.blockers["a"][0].kind, "quota");
}
#[test]
fn completion_releases_memory_even_on_persistence_failure() {
    let (s, _) = scheduler(&["a"]);
    let l = acquire(&s, "");
    s.accept(&l).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let p = temp.path().join("file");
    std::fs::write(&p, b"not directory").unwrap();
    s.guard().path = p.join("scheduler.json");
    assert!(s.complete(&l).is_err());
    assert!(s.guard().leases.is_empty());
    assert_eq!(s.guard().state.completed["a"], 1);
    s.complete(&l).unwrap();
    assert_eq!(s.guard().state.completed["a"], 1);
}
#[test]
fn go_state_null_maps_duration_keys_and_large_historical_bindings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.json");
    std::fs::write(&path,br#"{"version":1,"bindings":null,"activity":null,"blockers":null,"snapshots":null,"learning":null,"completed":null,"completed_models":null,"predicted_resets":null,"window_observed":null}"#).unwrap();
    let s = Scheduler::new(&path, pool(&["a"])).unwrap();
    done(&s, &acquire(&s, "session"));
    let w = window("week", 50.0, None, 10080);
    assert!(window_key(&w).ends_with("168h0m0s"));
    let mut another = w.clone();
    another.limit_id = "other".into();
    assert_ne!(window_key(&w), window_key(&another));
    let (s, c) = scheduler(&["a", "b"]);
    done(&s, &acquire(&s, "historic"));
    {
        let mut guard = s.guard();
        for i in 0..65537 {
            guard.state.bindings.insert(format!("idle-{i}"), "a".into());
        }
    }
    advance(&c, Duration::days(60));
    update(
        &s,
        "b",
        now(&c),
        vec![window(
            "short",
            0.0,
            Some(now(&c) + Duration::minutes(1)),
            300,
        )],
    );
    assert_eq!(acquire(&s, "historic").account.id, "a");
    assert_eq!(s.guard().state.bindings.len(), 65538);
}
#[test]
fn recent_bound_demand_reserves_expiry_without_expiring_binding() {
    let (s, c) = scheduler(&["a", "b"]);
    let t = now(&c);
    let a = window("five_hour", 95.0, Some(t + Duration::minutes(5)), 300);
    update(&s, "a", t, vec![a.clone()]);
    update(
        &s,
        "b",
        t,
        vec![window("five_hour", 20.0, Some(t + Duration::hours(4)), 300)],
    );
    let key = binding_key("codex", "busy");
    {
        let mut guard = s.guard();
        guard
            .state
            .learning
            .get_mut("a")
            .unwrap()
            .get_mut(&window_key(&a))
            .unwrap()
            .mean = 1.0;
        guard.state.bindings.insert(key.clone(), "a".into());
        guard.state.activity.insert(
            key.clone(),
            SessionActivity {
                account_id: "a".into(),
                model: MODEL.into(),
                last_request: t,
                cadence_seconds: 60.0,
            },
        );
    }
    assert_eq!(acquire(&s, "new").account.id, "b");
    assert_eq!(acquire(&s, "busy").account.id, "a");
    advance(&c, Duration::minutes(31));
    assert_eq!(s.guard().recent_demand("a", "", now(&c)), (0, 0.0));
    assert_eq!(s.guard().state.bindings[&key], "a");
}
