use serde::{Deserialize, Serialize};
use serde_json::json;
use specta::Type;
use specta_typescript::Typescript;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Manager;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};
use tauri_specta::collect_commands;
use tauri_svelte_synced_store::{
    StateSyncer, StateSyncerConfig, StateUpdate, SyncError, SyncMessage, state_handlers,
};

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct CounterState {
    count: u32,
    label: String,
}

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct OtherState {
    enabled: bool,
}

fn recording_channel() -> (Channel<SyncMessage>, Arc<Mutex<Vec<SyncMessage>>>) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&messages);
    let channel = Channel::new(move |body| {
        let InvokeResponseBody::Json(raw) = body else {
            panic!("expected JSON channel payload");
        };
        let parsed: SyncMessage = serde_json::from_str(&raw).expect("valid SyncMessage JSON");
        captured.lock().unwrap().push(parsed);
        Ok(())
    });
    (channel, messages)
}

fn build_mock_app(cfg: StateSyncerConfig) -> tauri::App<MockRuntime> {
    mock_builder()
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_svelte_synced_store::plugin(cfg))
        .build(mock_context(noop_assets()))
        .expect("build mock tauri app")
}

fn temp_store_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "tauri-svelte-synced-store-test-{}-{}-{unique}",
        std::process::id(),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("create temp test dir");
    dir.join("state.json")
}

#[test]
fn plugin_manages_state_syncer_regardless_of_store_plugin_order() {
    let path = temp_store_path("order");
    let cfg = StateSyncerConfig {
        filename: path.to_string_lossy().into_owned(),
        persist_keys: HashMap::from([("counter".to_string(), true)]),
        default_persist: false,
    };

    // Register synced-store plugin BEFORE tauri_plugin_store to verify lazy disk_store init.
    let app = mock_builder()
        .plugin(tauri_svelte_synced_store::plugin(cfg))
        .plugin(tauri_plugin_store::Builder::default().build())
        .build(mock_context(noop_assets()))
        .expect("build mock app with reversed plugin order");

    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "counter",
        CounterState {
            count: 7,
            label: "init".into(),
        },
    );
    assert_eq!(
        syncer.snapshot::<CounterState>("counter"),
        CounterState {
            count: 7,
            label: "init".into(),
        }
    );
}

#[test]
fn subscribe_delivers_initial_snapshot_and_ordered_updates() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "counter",
        CounterState {
            count: 1,
            label: "a".into(),
        },
    );

    let (ch, msgs) = recording_channel();
    let sub_id = syncer
        .subscribe_with_labels("counter", "main", "main", ch)
        .expect("subscribe succeeds");
    assert!(!sub_id.is_empty());
    assert_eq!(syncer.subscriber_count("counter"), 1);

    // Mutate via Item::drop
    {
        let item = syncer.get::<CounterState>("counter");
        let mut guard = item.lock().unwrap();
        guard.count = 2;
    }

    // Mutate via update(..., true)
    syncer.update(
        "counter",
        CounterState {
            count: 3,
            label: "b".into(),
        },
        true,
    );

    // Silent update(..., false) should NOT emit
    syncer.update(
        "counter",
        CounterState {
            count: 4,
            label: "silent".into(),
        },
        false,
    );

    // Explicit emit sends current state
    assert!(syncer.emit::<CounterState>("counter"));

    let received = msgs.lock().unwrap().clone();
    assert_eq!(
        received,
        vec![
            SyncMessage::Snapshot {
                value: json!({ "count": 1, "label": "a" })
            },
            SyncMessage::Snapshot {
                value: json!({ "count": 2, "label": "a" })
            },
            SyncMessage::Snapshot {
                value: json!({ "count": 3, "label": "b" })
            },
            SyncMessage::Snapshot {
                value: json!({ "count": 4, "label": "silent" })
            },
        ]
    );
}

#[test]
fn subscribe_unknown_key_returns_error_without_panicking() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    let (ch, msgs) = recording_channel();

    let err = syncer
        .subscribe_with_labels("missing", "main", "main", ch)
        .expect_err("unknown key must return Err");
    assert_eq!(err, SyncError::UnknownKey("missing".into()));
    assert!(msgs.lock().unwrap().is_empty());
}

#[test]
fn unsubscribe_checks_webview_ownership_and_stops_delivery() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set("counter", CounterState::default());

    let (ch, msgs) = recording_channel();
    let sub_id = syncer
        .subscribe_with_labels("counter", "webview-a", "win-1", ch)
        .unwrap();

    // Another webview cannot unsubscribe webview-a's subscription.
    assert!(!syncer.unsubscribe("counter", "webview-b", &sub_id).unwrap());
    assert_eq!(syncer.subscriber_count("counter"), 1);

    // Owning webview succeeds; second call is idempotent false.
    assert!(syncer.unsubscribe("counter", "webview-a", &sub_id).unwrap());
    assert!(!syncer.unsubscribe("counter", "webview-a", &sub_id).unwrap());
    assert_eq!(syncer.subscriber_count("counter"), 0);

    syncer.update(
        "counter",
        CounterState {
            count: 99,
            label: "after".into(),
        },
        true,
    );
    assert_eq!(msgs.lock().unwrap().len(), 1);
}

#[test]
fn clear_webview_and_clear_window_remove_only_matching_subscribers() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set("counter", CounterState::default());
    syncer.set("other", OtherState::default());

    let (ch1, msgs1) = recording_channel();
    let (ch2, msgs2) = recording_channel();
    let (ch3, msgs3) = recording_channel();
    let (ch4, msgs4) = recording_channel();

    syncer
        .subscribe_with_labels("counter", "wv-main", "win-1", ch1)
        .unwrap();
    syncer
        .subscribe_with_labels("other", "wv-main", "win-1", ch2)
        .unwrap();
    // Sub-webview in win-1 with distinct webview label
    syncer
        .subscribe_with_labels("counter", "wv-embed", "win-1", ch3)
        .unwrap();
    // Different window altogether
    syncer
        .subscribe_with_labels("counter", "wv-settings", "win-2", ch4)
        .unwrap();

    // Simulate reload of wv-main: removes ch1 and ch2, keeps wv-embed and wv-settings.
    assert_eq!(syncer.clear_webview("wv-main"), 2);
    assert_eq!(syncer.subscriber_count("counter"), 2);
    assert_eq!(syncer.subscriber_count("other"), 0);

    // Simulate win-1 destroy: removes remaining sub-webview wv-embed, keeps win-2.
    assert_eq!(syncer.clear_window("win-1"), 1);
    assert_eq!(syncer.subscriber_count("counter"), 1);

    syncer.update(
        "counter",
        CounterState {
            count: 5,
            label: "live".into(),
        },
        true,
    );

    assert_eq!(msgs1.lock().unwrap().len(), 1);
    assert_eq!(msgs2.lock().unwrap().len(), 1);
    assert_eq!(msgs3.lock().unwrap().len(), 1);
    assert_eq!(msgs4.lock().unwrap().len(), 2);
}

#[test]
fn update_from_frontend_validates_skips_origin_and_fans_out() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "counter",
        CounterState {
            count: 1,
            label: "init".into(),
        },
    );

    let (ch_a, msgs_a) = recording_channel();
    let (ch_b, msgs_b) = recording_channel();
    let sub_a = syncer
        .subscribe_with_labels("counter", "main", "main", ch_a)
        .unwrap();
    let _sub_b = syncer
        .subscribe_with_labels("counter", "secondary", "secondary", ch_b)
        .unwrap();

    // Invalid payload fails without mutating state or notifying subscribers.
    let err = syncer
        .update_from_frontend(StateUpdate {
            name: "counter".into(),
            value: json!({ "count": "not-a-number", "label": "bad" }),
            subscriber_id: Some(sub_a.clone()),
        })
        .expect_err("invalid JSON shape must fail");
    assert!(matches!(err, SyncError::InvalidValue(_)));
    assert_eq!(syncer.snapshot::<CounterState>("counter").count, 1);
    assert_eq!(msgs_a.lock().unwrap().len(), 1);
    assert_eq!(msgs_b.lock().unwrap().len(), 1);

    // Valid update skips origin sub_a and delivers to sub_b.
    syncer
        .update_from_frontend(StateUpdate {
            name: "counter".into(),
            value: json!({ "count": 42, "label": "updated" }),
            subscriber_id: Some(sub_a),
        })
        .expect("valid frontend update succeeds");

    assert_eq!(
        syncer.snapshot::<CounterState>("counter"),
        CounterState {
            count: 42,
            label: "updated".into(),
        }
    );
    assert_eq!(msgs_a.lock().unwrap().len(), 1);
    assert_eq!(msgs_b.lock().unwrap().len(), 2);
    assert_eq!(
        msgs_b.lock().unwrap()[1],
        SyncMessage::Snapshot {
            value: json!({ "count": 42, "label": "updated" })
        }
    );
}

#[test]
fn set_on_existing_key_preserves_subscribers_and_stays_silent() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set("counter", CounterState::default());

    let (ch, msgs) = recording_channel();
    syncer
        .subscribe_with_labels("counter", "main", "main", ch)
        .unwrap();

    // Silent overwrite preserves the existing subscriber.
    syncer.set(
        "counter",
        CounterState {
            count: 10,
            label: "silent".into(),
        },
    );
    assert_eq!(syncer.subscriber_count("counter"), 1);
    assert_eq!(msgs.lock().unwrap().len(), 1);

    // Subsequent emit reaches the preserved subscriber with the updated value.
    assert!(syncer.emit::<CounterState>("counter"));
    assert_eq!(
        msgs.lock().unwrap()[1],
        SyncMessage::Snapshot {
            value: json!({ "count": 10, "label": "silent" })
        }
    );
}

#[test]
fn failed_channel_send_prunes_dead_subscriber_without_panicking() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set("counter", CounterState::default());

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = Arc::clone(&attempts);
    let failing_ch = Channel::new(move |_body| {
        let n = attempts_clone.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            Ok(())
        } else {
            Err(tauri::Error::WebviewNotFound)
        }
    });
    let (healthy_ch, healthy_msgs) = recording_channel();

    syncer
        .subscribe_with_labels("counter", "dead-wv", "win-dead", failing_ch)
        .unwrap();
    syncer
        .subscribe_with_labels("counter", "live-wv", "win-live", healthy_ch)
        .unwrap();
    assert_eq!(syncer.subscriber_count("counter"), 2);

    // First publish after subscribe fails on dead-wv and prunes it while delivering to live-wv.
    {
        let item = syncer.get::<CounterState>("counter");
        item.lock().unwrap().count = 1;
    }

    assert_eq!(syncer.subscriber_count("counter"), 1);
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(healthy_msgs.lock().unwrap().len(), 2);

    // Next publish only touches live-wv.
    syncer.update(
        "counter",
        CounterState {
            count: 2,
            label: "ok".into(),
        },
        true,
    );
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(healthy_msgs.lock().unwrap().len(), 3);
}

#[test]
fn item_drop_during_unwind_skips_publish_and_recovers_cleanly() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "counter",
        CounterState {
            count: 1,
            label: "clean".into(),
        },
    );

    let (ch, msgs) = recording_channel();
    syncer
        .subscribe_with_labels("counter", "main", "main", ch)
        .unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let item = syncer.get::<CounterState>("counter");
        let mut guard = item.lock().unwrap();
        guard.count = 999;
        panic!("simulated failure while mutating state");
    }));
    assert!(result.is_err());

    // Unwind skipped publishing the torn mutation.
    assert_eq!(msgs.lock().unwrap().len(), 1);

    // Subsequent update still recovers the poisoned lock and publishes normally.
    syncer.update(
        "counter",
        CounterState {
            count: 2,
            label: "recovered".into(),
        },
        true,
    );
    assert_eq!(msgs.lock().unwrap().len(), 2);
    assert_eq!(
        msgs.lock().unwrap()[1],
        SyncMessage::Snapshot {
            value: json!({ "count": 2, "label": "recovered" })
        }
    );
}

#[test]
fn disk_persistence_round_trips_through_store_plugin() {
    let path = temp_store_path("persist");
    let cfg = StateSyncerConfig {
        filename: path.to_string_lossy().into_owned(),
        persist_keys: HashMap::from([("counter".to_string(), true)]),
        default_persist: false,
    };

    {
        let app = build_mock_app(cfg.clone());
        let syncer = app.state::<StateSyncer<MockRuntime>>();
        syncer.set(
            "counter",
            CounterState {
                count: 55,
                label: "saved".into(),
            },
        );
        // Flush store to disk explicitly via tauri-plugin-store API.
        use tauri_plugin_store::StoreExt;
        app.store(&path).unwrap().save().unwrap();
    }

    let app2 = build_mock_app(cfg);
    let syncer2 = app2.state::<StateSyncer<MockRuntime>>();
    let loaded = syncer2.load::<CounterState>("counter");
    assert_eq!(
        loaded,
        CounterState {
            count: 55,
            label: "saved".into(),
        }
    );
}

mod specta_macro_test {
    use super::*;
    state_handlers!();

    #[test]
    fn specta_exports_macro_and_module_commands_with_channel_types() {
        let out_macro = temp_store_path("bindings-macro").with_extension("ts");
        tauri_specta::Builder::<tauri::Wry>::new()
            .commands(collect_commands![
                subscribe_state,
                unsubscribe_state,
                update_state,
            ])
            .export(Typescript::default(), &out_macro)
            .expect("export macro commands bindings");

        let macro_ts = std::fs::read_to_string(&out_macro).unwrap();
        assert!(macro_ts.contains("Channel<SyncMessage>"), "{macro_ts}");
        assert!(macro_ts.contains("subscribeState"), "{macro_ts}");
        assert!(macro_ts.contains("unsubscribeState"), "{macro_ts}");
        assert!(macro_ts.contains("updateState"), "{macro_ts}");
        assert!(macro_ts.contains("StateUpdate"), "{macro_ts}");
        assert!(macro_ts.contains("SyncMessage"), "{macro_ts}");
        assert!(macro_ts.contains("SyncError"), "{macro_ts}");

        let out_module = temp_store_path("bindings-module").with_extension("ts");
        tauri_specta::Builder::<tauri::Wry>::new()
            .commands(collect_commands![
                tauri_svelte_synced_store::commands::subscribe_state,
                tauri_svelte_synced_store::commands::unsubscribe_state,
                tauri_svelte_synced_store::commands::update_state,
            ])
            .export(Typescript::default(), &out_module)
            .expect("export module commands bindings");

        let module_ts = std::fs::read_to_string(&out_module).unwrap();
        assert!(module_ts.contains("Channel<SyncMessage>"), "{module_ts}");
        assert!(module_ts.contains("subscribeState"), "{module_ts}");
        assert!(module_ts.contains("unsubscribeState"), "{module_ts}");
        assert!(module_ts.contains("updateState"), "{module_ts}");
    }
}
