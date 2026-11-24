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
    DeliveryMode, JsonPatchOp, StateSnapshot, StateSyncer, StateSyncerConfig, StateUpdate,
    SyncError, SyncMessage, UpdateOutcome, UpdatePayload, state_handlers,
};

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct CounterState {
    count: u32,
    label: String,
}

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct RichState {
    count: u32,
    label: String,
    padding: String,
    nested: NestedMeta,
}

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct NestedMeta {
    enabled: bool,
    tag: String,
}

#[derive(Serialize, Deserialize, Type, Clone, Debug, Default, PartialEq)]
struct OtherState {
    enabled: bool,
}

#[derive(Serialize, Type, Clone, Debug, Default, PartialEq)]
struct NormalizedState {
    name: String,
    count: u32,
}

impl<'de> Deserialize<'de> for NormalizedState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            count: u32,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            name: raw.name.trim().to_owned(),
            count: raw.count,
        })
    }
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
        ..Default::default()
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
    assert_eq!(syncer.revision("counter").unwrap(), 0);
}

#[test]
fn subscribe_delivers_initial_snapshot_then_compact_deltas_and_proactive_snapshot_after_silent_update()
 {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "rich",
        RichState {
            count: 1,
            label: "alpha".into(),
            padding: "x".repeat(256),
            nested: NestedMeta {
                enabled: false,
                tag: "v1".into(),
            },
        },
    );

    let (ch, msgs) = recording_channel();
    let sub_id = syncer
        .subscribe_with_labels("rich", "main", "main", ch, DeliveryMode::Delta)
        .expect("subscribe succeeds");
    assert!(!sub_id.is_empty());
    assert_eq!(syncer.subscriber_count("rich"), 1);

    // Mutate via Item::drop -> emits compact Delta (rev 0 -> 1)
    {
        let item = syncer.get::<RichState>("rich");
        let mut guard = item.lock().unwrap();
        guard.count = 2;
    }

    // Mutate via update(..., true) -> emits compact Delta (rev 1 -> 2)
    let mut next = syncer.snapshot::<RichState>("rich");
    next.nested.enabled = true;
    syncer.update("rich", next.clone(), true);

    // Silent update(..., false) bumps revision (rev 2 -> 3) without emitting
    next.label = "silent".into();
    syncer.update("rich", next.clone(), false);
    assert_eq!(syncer.revision("rich").unwrap(), 3);
    assert_eq!(msgs.lock().unwrap().len(), 3);

    // Next emitting update (rev 3 -> 4) sees sub is still at rev 2 != 3, so proactively sends Snapshot
    next.count = 4;
    syncer.update("rich", next.clone(), true);

    // Following update (rev 4 -> 5) resumes sending compact Delta because sub is back at rev 4
    next.nested.tag = "v2".into();
    syncer.update("rich", next.clone(), true);

    let received = msgs.lock().unwrap().clone();
    assert_eq!(received.len(), 5);
    assert!(matches!(
        received[0],
        SyncMessage::Snapshot { revision: 0, .. }
    ));
    assert_eq!(
        received[1],
        SyncMessage::Delta {
            base_revision: 0,
            revision: 1,
            patch: vec![JsonPatchOp::Replace {
                path: "/count".into(),
                value: json!(2),
            }],
        }
    );
    assert_eq!(
        received[2],
        SyncMessage::Delta {
            base_revision: 1,
            revision: 2,
            patch: vec![JsonPatchOp::Replace {
                path: "/nested/enabled".into(),
                value: json!(true),
            }],
        }
    );
    assert!(matches!(
        &received[3],
        SyncMessage::Snapshot { revision: 4, value }
            if value["count"] == json!(4) && value["label"] == json!("silent")
    ));
    assert_eq!(
        received[4],
        SyncMessage::Delta {
            base_revision: 4,
            revision: 5,
            patch: vec![JsonPatchOp::Replace {
                path: "/nested/tag".into(),
                value: json!("v2"),
            }],
        }
    );
}

#[test]
fn noop_mutations_suppress_publish_and_revision_bump_while_explicit_emit_forces_snapshot() {
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
    syncer
        .subscribe_with_labels("counter", "main", "main", ch, DeliveryMode::Delta)
        .unwrap();

    // Read-only Item::drop does not bump revision or emit
    {
        let item = syncer.get::<CounterState>("counter");
        assert_eq!(item.lock().unwrap().count, 1);
    }
    assert_eq!(syncer.revision("counter").unwrap(), 0);
    assert_eq!(msgs.lock().unwrap().len(), 1);

    // update(..., true) with identical value does not bump revision or emit
    syncer.update(
        "counter",
        CounterState {
            count: 1,
            label: "a".into(),
        },
        true,
    );
    assert_eq!(syncer.revision("counter").unwrap(), 0);
    assert_eq!(msgs.lock().unwrap().len(), 1);

    // Explicit emit always broadcasts a Snapshot at the current revision
    assert!(syncer.emit::<CounterState>("counter"));
    assert_eq!(syncer.revision("counter").unwrap(), 0);
    assert_eq!(
        msgs.lock().unwrap()[1],
        SyncMessage::Snapshot {
            revision: 0,
            value: json!({ "count": 1, "label": "a" }),
        }
    );
}

#[test]
fn adaptive_fallback_and_per_key_or_per_subscriber_snapshot_modes() {
    let cfg = StateSyncerConfig {
        delivery_modes: HashMap::from([("snapshot_only".to_string(), DeliveryMode::Snapshot)]),
        ..Default::default()
    };
    let app = build_mock_app(cfg);
    let syncer = app.state::<StateSyncer<MockRuntime>>();

    // Tiny struct where JSON patch is larger than full JSON -> falls back to Snapshot automatically
    syncer.set(
        "tiny",
        CounterState {
            count: 1,
            label: "a".into(),
        },
    );
    let (tiny_ch, tiny_msgs) = recording_channel();
    syncer
        .subscribe_with_labels("tiny", "main", "main", tiny_ch, DeliveryMode::Delta)
        .unwrap();
    syncer.update(
        "tiny",
        CounterState {
            count: 2,
            label: "a".into(),
        },
        true,
    );
    assert_eq!(
        tiny_msgs.lock().unwrap()[1],
        SyncMessage::Snapshot {
            revision: 1,
            value: json!({ "count": 2, "label": "a" }),
        }
    );

    // Key configured as DeliveryMode::Snapshot forces Snapshot even for Delta subscribers
    syncer.set(
        "snapshot_only",
        RichState {
            count: 1,
            label: "a".into(),
            padding: "p".repeat(256),
            nested: NestedMeta::default(),
        },
    );
    let (so_ch, so_msgs) = recording_channel();
    syncer
        .subscribe_with_labels("snapshot_only", "main", "main", so_ch, DeliveryMode::Delta)
        .unwrap();
    {
        let item = syncer.get::<RichState>("snapshot_only");
        item.lock().unwrap().count = 2;
    }
    assert!(matches!(
        so_msgs.lock().unwrap()[1],
        SyncMessage::Snapshot { revision: 1, .. }
    ));

    // Mixed subscribers on a delta-enabled key: Delta sub gets Delta, Snapshot sub gets Snapshot
    syncer.set(
        "mixed",
        RichState {
            count: 1,
            label: "a".into(),
            padding: "p".repeat(256),
            nested: NestedMeta::default(),
        },
    );
    let (delta_ch, delta_msgs) = recording_channel();
    let (snap_ch, snap_msgs) = recording_channel();
    syncer
        .subscribe_with_labels("mixed", "wv-delta", "win-1", delta_ch, DeliveryMode::Delta)
        .unwrap();
    syncer
        .subscribe_with_labels("mixed", "wv-snap", "win-2", snap_ch, DeliveryMode::Snapshot)
        .unwrap();

    {
        let item = syncer.get::<RichState>("mixed");
        item.lock().unwrap().count = 9;
    }
    assert!(matches!(
        delta_msgs.lock().unwrap()[1],
        SyncMessage::Delta {
            base_revision: 0,
            revision: 1,
            ..
        }
    ));
    assert!(matches!(
        snap_msgs.lock().unwrap()[1],
        SyncMessage::Snapshot { revision: 1, .. }
    ));
}

#[test]
fn subscribe_unknown_key_returns_error_without_panicking() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    let (ch, msgs) = recording_channel();

    let err = syncer
        .subscribe_with_labels("missing", "main", "main", ch, DeliveryMode::Delta)
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
        .subscribe_with_labels("counter", "webview-a", "win-1", ch, DeliveryMode::Delta)
        .unwrap();

    assert!(!syncer.unsubscribe("counter", "webview-b", &sub_id).unwrap());
    assert_eq!(syncer.subscriber_count("counter"), 1);

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
        .subscribe_with_labels("counter", "wv-main", "win-1", ch1, DeliveryMode::Delta)
        .unwrap();
    syncer
        .subscribe_with_labels("other", "wv-main", "win-1", ch2, DeliveryMode::Delta)
        .unwrap();
    syncer
        .subscribe_with_labels("counter", "wv-embed", "win-1", ch3, DeliveryMode::Delta)
        .unwrap();
    syncer
        .subscribe_with_labels("counter", "wv-settings", "win-2", ch4, DeliveryMode::Delta)
        .unwrap();

    assert_eq!(syncer.clear_webview("wv-main"), 2);
    assert_eq!(syncer.subscriber_count("counter"), 2);
    assert_eq!(syncer.subscriber_count("other"), 0);

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
fn update_from_frontend_handles_delta_full_validation_stale_and_normalization() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "rich",
        RichState {
            count: 1,
            label: "init".into(),
            padding: "p".repeat(256),
            nested: NestedMeta {
                enabled: false,
                tag: "v1".into(),
            },
        },
    );

    let (ch_a, msgs_a) = recording_channel();
    let (ch_b, msgs_b) = recording_channel();
    let sub_a = syncer
        .subscribe_with_labels("rich", "main", "main", ch_a, DeliveryMode::Delta)
        .unwrap();
    let _sub_b = syncer
        .subscribe_with_labels("rich", "secondary", "secondary", ch_b, DeliveryMode::Delta)
        .unwrap();

    // Invalid patch returns InvalidPatch without mutating state
    let err = syncer
        .update_from_frontend(StateUpdate {
            name: "rich".into(),
            base_revision: 0,
            payload: UpdatePayload::Delta {
                patch: vec![JsonPatchOp::Replace {
                    path: "/does_not_exist".into(),
                    value: json!(1),
                }],
            },
            subscriber_id: Some(sub_a.clone()),
        })
        .expect_err("invalid patch must fail");
    assert!(matches!(err, SyncError::InvalidPatch(_)));
    assert_eq!(syncer.revision("rich").unwrap(), 0);

    // Invalid value shape returns InvalidValue without mutating state
    let err = syncer
        .update_from_frontend(StateUpdate {
            name: "rich".into(),
            base_revision: 0,
            payload: UpdatePayload::Delta {
                patch: vec![JsonPatchOp::Replace {
                    path: "/count".into(),
                    value: json!("not-a-number"),
                }],
            },
            subscriber_id: Some(sub_a.clone()),
        })
        .expect_err("invalid type must fail");
    assert!(matches!(err, SyncError::InvalidValue(_)));
    assert_eq!(syncer.revision("rich").unwrap(), 0);

    // Valid delta skips origin sub_a and delivers delta to sub_b
    let outcome = syncer
        .update_from_frontend(StateUpdate {
            name: "rich".into(),
            base_revision: 0,
            payload: UpdatePayload::Delta {
                patch: vec![JsonPatchOp::Replace {
                    path: "/count".into(),
                    value: json!(42),
                }],
            },
            subscriber_id: Some(sub_a.clone()),
        })
        .expect("valid delta succeeds");
    assert_eq!(
        outcome,
        UpdateOutcome::Synced {
            revision: 1,
            canonical: None,
        }
    );
    assert_eq!(syncer.snapshot::<RichState>("rich").count, 42);
    assert_eq!(msgs_a.lock().unwrap().len(), 1);
    assert_eq!(
        msgs_b.lock().unwrap()[1],
        SyncMessage::Delta {
            base_revision: 0,
            revision: 1,
            patch: vec![JsonPatchOp::Replace {
                path: "/count".into(),
                value: json!(42),
            }],
        }
    );

    // No-op update at current revision returns Unchanged without bumping revision
    let unchanged = syncer
        .update_from_frontend(StateUpdate {
            name: "rich".into(),
            base_revision: 1,
            payload: UpdatePayload::Delta {
                patch: vec![JsonPatchOp::Replace {
                    path: "/count".into(),
                    value: json!(42),
                }],
            },
            subscriber_id: Some(sub_a.clone()),
        })
        .unwrap();
    assert_eq!(unchanged, UpdateOutcome::Unchanged { revision: 1 });
    assert_eq!(syncer.revision("rich").unwrap(), 1);

    // Stale base_revision returns Stale with latest snapshot and does not mutate
    let stale = syncer
        .update_from_frontend(StateUpdate {
            name: "rich".into(),
            base_revision: 0,
            payload: UpdatePayload::Delta {
                patch: vec![JsonPatchOp::Replace {
                    path: "/label".into(),
                    value: json!("stale-write"),
                }],
            },
            subscriber_id: Some(sub_a.clone()),
        })
        .unwrap();
    assert!(matches!(
        stale,
        UpdateOutcome::Stale {
            snapshot: StateSnapshot { revision: 1, ref value }
        } if value["count"] == json!(42)
    ));
    assert_eq!(syncer.snapshot::<RichState>("rich").label, "init");

    // Custom Deserialize normalization returns canonical value in Synced outcome
    syncer.set(
        "norm",
        NormalizedState {
            name: "alice".into(),
            count: 1,
        },
    );
    let norm_outcome = syncer
        .update_from_frontend(StateUpdate {
            name: "norm".into(),
            base_revision: 0,
            payload: UpdatePayload::Full {
                value: json!({ "name": "  bob  ", "count": 2 }),
            },
            subscriber_id: None,
        })
        .unwrap();
    assert_eq!(
        norm_outcome,
        UpdateOutcome::Synced {
            revision: 1,
            canonical: Some(json!({ "name": "bob", "count": 2 })),
        }
    );
}

#[test]
fn sync_snapshot_returns_current_revision_and_resets_subscriber_tracking_for_deltas() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    let mut state = RichState {
        count: 1,
        label: "a".into(),
        padding: "p".repeat(256),
        nested: NestedMeta::default(),
    };
    syncer.set("rich", state.clone());

    let (ch, msgs) = recording_channel();
    let sub_id = syncer
        .subscribe_with_labels("rich", "main", "main", ch, DeliveryMode::Delta)
        .unwrap();

    // Silent update moves rev 0 -> 1 without notifying sub
    state.count = 2;
    syncer.update("rich", state.clone(), false);

    // Explicit sync_snapshot brings sub tracking up to rev 1
    let snap = syncer
        .sync_snapshot("rich", Some("main"), Some(&sub_id))
        .unwrap();
    assert_eq!(snap.revision, 1);
    assert_eq!(snap.value["count"], json!(2));

    // Next publish (rev 1 -> 2) can now deliver a compact Delta instead of a Snapshot
    state.count = 3;
    syncer.update("rich", state, true);
    assert_eq!(
        msgs.lock().unwrap()[1],
        SyncMessage::Delta {
            base_revision: 1,
            revision: 2,
            patch: vec![JsonPatchOp::Replace {
                path: "/count".into(),
                value: json!(3),
            }],
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
        .subscribe_with_labels("counter", "main", "main", ch, DeliveryMode::Delta)
        .unwrap();

    syncer.set(
        "counter",
        CounterState {
            count: 10,
            label: "silent".into(),
        },
    );
    assert_eq!(syncer.subscriber_count("counter"), 1);
    assert_eq!(syncer.revision("counter").unwrap(), 1);
    assert_eq!(msgs.lock().unwrap().len(), 1);

    assert!(syncer.emit::<CounterState>("counter"));
    assert_eq!(
        msgs.lock().unwrap()[1],
        SyncMessage::Snapshot {
            revision: 1,
            value: json!({ "count": 10, "label": "silent" }),
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
        .subscribe_with_labels(
            "counter",
            "dead-wv",
            "win-dead",
            failing_ch,
            DeliveryMode::Delta,
        )
        .unwrap();
    syncer
        .subscribe_with_labels(
            "counter",
            "live-wv",
            "win-live",
            healthy_ch,
            DeliveryMode::Delta,
        )
        .unwrap();
    assert_eq!(syncer.subscriber_count("counter"), 2);

    {
        let item = syncer.get::<CounterState>("counter");
        item.lock().unwrap().count = 1;
    }

    assert_eq!(syncer.subscriber_count("counter"), 1);
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(healthy_msgs.lock().unwrap().len(), 2);

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
        .subscribe_with_labels("counter", "main", "main", ch, DeliveryMode::Delta)
        .unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let item = syncer.get::<CounterState>("counter");
        let mut guard = item.lock().unwrap();
        guard.count = 999;
        panic!("simulated failure while mutating state");
    }));
    assert!(result.is_err());

    assert_eq!(msgs.lock().unwrap().len(), 1);
    assert_eq!(syncer.revision("counter").unwrap(), 0);

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
            revision: 1,
            value: json!({ "count": 2, "label": "recovered" }),
        }
    );
}

#[test]
fn large_state_single_field_mutation_emits_tiny_delta() {
    let app = build_mock_app(StateSyncerConfig::default());
    let syncer = app.state::<StateSyncer<MockRuntime>>();
    syncer.set(
        "large",
        RichState {
            count: 0,
            label: "big".into(),
            padding: "x".repeat(2_000_000),
            nested: NestedMeta::default(),
        },
    );

    let (ch, msgs) = recording_channel();
    syncer
        .subscribe_with_labels("large", "main", "main", ch, DeliveryMode::Delta)
        .unwrap();

    {
        let item = syncer.get::<RichState>("large");
        item.lock().unwrap().count = 1;
    }

    let delta_msg = &msgs.lock().unwrap()[1];
    let wire_bytes = serde_json::to_vec(delta_msg).unwrap().len();
    assert!(
        wire_bytes < 200,
        "expected tiny delta, got {wire_bytes} bytes"
    );
    assert_eq!(
        *delta_msg,
        SyncMessage::Delta {
            base_revision: 0,
            revision: 1,
            patch: vec![JsonPatchOp::Replace {
                path: "/count".into(),
                value: json!(1),
            }],
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
        ..Default::default()
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
    fn specta_exports_macro_and_module_commands_with_delta_types() {
        let out_macro = temp_store_path("bindings-macro").with_extension("ts");
        tauri_specta::Builder::<tauri::Wry>::new()
            .commands(collect_commands![
                subscribe_state,
                unsubscribe_state,
                snapshot_state,
                update_state,
            ])
            .export(Typescript::default(), &out_macro)
            .expect("export macro commands bindings");

        let macro_ts = std::fs::read_to_string(&out_macro).unwrap();
        for expected in [
            "Channel<SyncMessage>",
            "subscribeState",
            "unsubscribeState",
            "snapshotState",
            "updateState",
            "DeliveryMode",
            "JsonPatchOp",
            "StateSnapshot",
            "UpdatePayload",
            "StateUpdate",
            "UpdateOutcome",
            "SyncMessage",
            "SyncError",
            "baseRevision",
        ] {
            assert!(
                macro_ts.contains(expected),
                "macro bindings missing '{expected}':\n{macro_ts}"
            );
        }

        let out_module = temp_store_path("bindings-module").with_extension("ts");
        tauri_specta::Builder::<tauri::Wry>::new()
            .commands(collect_commands![
                tauri_svelte_synced_store::commands::subscribe_state,
                tauri_svelte_synced_store::commands::unsubscribe_state,
                tauri_svelte_synced_store::commands::snapshot_state,
                tauri_svelte_synced_store::commands::update_state,
            ])
            .export(Typescript::default(), &out_module)
            .expect("export module commands bindings");

        let module_ts = std::fs::read_to_string(&out_module).unwrap();
        for expected in [
            "Channel<SyncMessage>",
            "subscribeState",
            "unsubscribeState",
            "snapshotState",
            "updateState",
            "UpdateOutcome",
            "baseRevision",
        ] {
            assert!(
                module_ts.contains(expected),
                "module bindings missing '{expected}':\n{module_ts}"
            );
        }
    }
}
