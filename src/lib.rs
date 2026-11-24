use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, LockResult, Mutex, MutexGuard, PoisonError};
use tauri::ipc::Channel;
use tauri::{AppHandle, Runtime, Webview};
use tauri_plugin_store::{Store, StoreExt};
use tracing::{debug, error, info, warn};

pub mod commands;
mod plugin;
mod protocol;

pub use plugin::plugin;
pub use protocol::{JsonValue, StateUpdate, SyncError, SyncMessage};

static VERBOSE_LOG: LazyLock<bool> = LazyLock::new(|| std::env::var("RUST_DEBUG").is_ok());

pub fn debug_val<T: Debug>(val: &T) -> String {
    if *VERBOSE_LOG {
        format!("{val:?}")
    } else {
        "<...>".to_string()
    }
}

pub trait ItemTrait: 'static + Send + Sync + Serialize + DeserializeOwned + Debug + Clone {}

impl<T> ItemTrait for T where T: 'static + Send + Sync + Serialize + DeserializeOwned + Debug + Clone
{}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Subscriber {
    id: String,
    webview_label: String,
    window_label: String,
    channel: Channel<SyncMessage>,
}

#[derive(Default)]
struct SubscriberRegistry {
    subscribers: Vec<Subscriber>,
}

impl SubscriberRegistry {
    /// Publishes a snapshot to all subscribers except `exclude_subscriber_id`.
    /// Returns any subscribers whose channel failed so they can be dropped outside the lock.
    fn publish(
        &mut self,
        value: &serde_json::Value,
        exclude_subscriber_id: Option<&str>,
    ) -> Vec<Subscriber> {
        let mut dead = Vec::new();
        let mut idx = 0;

        while idx < self.subscribers.len() {
            if exclude_subscriber_id == Some(self.subscribers[idx].id.as_str()) {
                idx += 1;
                continue;
            }

            let msg = SyncMessage::Snapshot {
                value: value.clone(),
            };
            match self.subscribers[idx].channel.send(msg) {
                Ok(()) => {
                    idx += 1;
                }
                Err(err) => {
                    let removed = self.subscribers.remove(idx);
                    warn!(
                        subscriber_id = %removed.id,
                        webview = %removed.webview_label,
                        error = %err,
                        "dropping subscriber after channel send failure"
                    );
                    dead.push(removed);
                }
            }
        }

        dead
    }

    fn remove_by_id(&mut self, webview_label: &str, subscriber_id: &str) -> Option<Subscriber> {
        let pos = self
            .subscribers
            .iter()
            .position(|sub| sub.id == subscriber_id && sub.webview_label == webview_label)?;
        Some(self.subscribers.remove(pos))
    }

    fn remove_by_webview(&mut self, webview_label: &str) -> Vec<Subscriber> {
        let mut removed = Vec::new();
        let mut idx = 0;
        while idx < self.subscribers.len() {
            if self.subscribers[idx].webview_label == webview_label {
                removed.push(self.subscribers.remove(idx));
            } else {
                idx += 1;
            }
        }
        removed
    }

    fn remove_by_window(&mut self, window_label: &str) -> Vec<Subscriber> {
        let mut removed = Vec::new();
        let mut idx = 0;
        while idx < self.subscribers.len() {
            if self.subscribers[idx].window_label == window_label {
                removed.push(self.subscribers.remove(idx));
            } else {
                idx += 1;
            }
        }
        removed
    }
}

struct TypedEntry<T: ItemTrait> {
    value: Mutex<T>,
    subscribers: Mutex<SubscriberRegistry>,
}

impl<T: ItemTrait> TypedEntry<T> {
    fn new(value: T) -> Self {
        Self {
            value: Mutex::new(value),
            subscribers: Mutex::new(SubscriberRegistry::default()),
        }
    }
}

trait ErasedEntry: Any + Send + Sync {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn snapshot_json(&self) -> Result<serde_json::Value, SyncError>;
    fn update_from_json(&self, value: serde_json::Value) -> Result<serde_json::Value, SyncError>;
    fn subscribers(&self) -> &Mutex<SubscriberRegistry>;
    fn type_name(&self) -> &'static str;
}

impl<T: ItemTrait> ErasedEntry for TypedEntry<T> {
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn snapshot_json(&self) -> Result<serde_json::Value, SyncError> {
        let guard = lock_recover(&self.value);
        serde_json::to_value(&*guard).map_err(|err| SyncError::SerializeFailed(err.to_string()))
    }

    fn update_from_json(&self, value: serde_json::Value) -> Result<serde_json::Value, SyncError> {
        let parsed: T = serde_json::from_value(value)
            .map_err(|err| SyncError::InvalidValue(err.to_string()))?;
        let canonical = serde_json::to_value(&parsed)
            .map_err(|err| SyncError::SerializeFailed(err.to_string()))?;
        let mut guard = lock_recover(&self.value);
        *guard = parsed;
        Ok(canonical)
    }

    fn subscribers(&self) -> &Mutex<SubscriberRegistry> {
        &self.subscribers
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

/// Guard handle returned by `StateSyncer::get` that publishes and persists state on drop.
pub struct Item<T: ItemTrait, R: Runtime = tauri::Wry> {
    entry: Arc<TypedEntry<T>>,
    key: String,
    syncer: StateSyncer<R>,
}

impl<T: ItemTrait, R: Runtime> Item<T, R> {
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        self.entry.value.lock()
    }
}

impl<T: ItemTrait, R: Runtime> Drop for Item<T, R> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            warn!(
                key = %self.key,
                "skipping synced state publish and persistence during unwind"
            );
            return;
        }

        self.syncer
            .publish_typed_entry(&self.key, &self.entry, true);
    }
}

impl<T: ItemTrait, R: Runtime> Clone for Item<T, R> {
    fn clone(&self) -> Self {
        Self {
            entry: Arc::clone(&self.entry),
            key: self.key.clone(),
            syncer: self.syncer.clone(),
        }
    }
}

impl<T: ItemTrait + PartialEq, R: Runtime> PartialEq for Item<T, R> {
    fn eq(&self, other: &Self) -> bool {
        if self.key != other.key {
            return false;
        }
        if Arc::ptr_eq(&self.entry, &other.entry) {
            return true;
        }

        // Lock in pointer-address order so concurrent `a == b` and `b == a` cannot deadlock.
        let self_ptr = Arc::as_ptr(&self.entry) as usize;
        let other_ptr = Arc::as_ptr(&other.entry) as usize;
        if self_ptr < other_ptr {
            let self_guard = lock_recover(&self.entry.value);
            let other_guard = lock_recover(&other.entry.value);
            *self_guard == *other_guard
        } else {
            let other_guard = lock_recover(&other.entry.value);
            let self_guard = lock_recover(&self.entry.value);
            *self_guard == *other_guard
        }
    }
}

#[derive(Clone, Debug)]
pub struct StateSyncerConfig {
    pub filename: String,
    pub persist_keys: HashMap<String, bool>,
    pub default_persist: bool,
}

impl Default for StateSyncerConfig {
    fn default() -> Self {
        Self {
            filename: "state.json".to_owned(),
            persist_keys: HashMap::new(),
            default_persist: false,
        }
    }
}

type EntriesMap = HashMap<String, Arc<dyn ErasedEntry>>;

pub struct StateSyncer<R: Runtime = tauri::Wry> {
    entries: Arc<Mutex<EntriesMap>>,
    next_subscriber_id: Arc<AtomicU64>,
    app: AppHandle<R>,
    cfg: Arc<StateSyncerConfig>,
    disk_store: Arc<Mutex<Option<Arc<Store<R>>>>>,
}

impl<R: Runtime> Clone for StateSyncer<R> {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
            next_subscriber_id: Arc::clone(&self.next_subscriber_id),
            app: self.app.clone(),
            cfg: Arc::clone(&self.cfg),
            disk_store: Arc::clone(&self.disk_store),
        }
    }
}

impl<R: Runtime> StateSyncer<R> {
    pub fn new(cfg: StateSyncerConfig, app: AppHandle<R>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            next_subscriber_id: Arc::new(AtomicU64::new(1)),
            app,
            cfg: Arc::new(cfg),
            disk_store: Arc::new(Mutex::new(None)),
        }
    }

    fn should_persist(&self, key: &str) -> bool {
        *self
            .cfg
            .persist_keys
            .get(key)
            .unwrap_or(&self.cfg.default_persist)
    }

    fn disk_store(&self) -> Result<Arc<Store<R>>, SyncError> {
        let mut cache = lock_recover(&self.disk_store);
        if let Some(store) = cache.as_ref() {
            return Ok(Arc::clone(store));
        }

        let store = self
            .app
            .store(&self.cfg.filename)
            .map_err(|err| SyncError::PersistenceFailed(err.to_string()))?;
        *cache = Some(Arc::clone(&store));
        Ok(store)
    }

    fn persist_json(&self, key: &str, value: serde_json::Value) -> Result<(), SyncError> {
        let store = self.disk_store()?;
        store.set(key, value);
        Ok(())
    }

    fn get_erased_entry(&self, key: &str) -> Result<Arc<dyn ErasedEntry>, SyncError> {
        let entries = lock_recover(&self.entries);
        entries
            .get(key)
            .cloned()
            .ok_or_else(|| SyncError::UnknownKey(key.to_string()))
    }

    fn get_typed_entry<T: ItemTrait>(&self, key: &str) -> Result<Arc<TypedEntry<T>>, SyncError> {
        let entry = self.get_erased_entry(key)?;
        let expected = std::any::type_name::<T>();
        let actual = entry.type_name();
        if entry.as_any().is::<TypedEntry<T>>() {
            let erased_any: Arc<dyn Any + Send + Sync> = entry;
            erased_any.downcast::<TypedEntry<T>>().map_err(|_| {
                SyncError::TypeMismatch(format!("{key}: expected {expected}, found {actual}"))
            })
        } else {
            Err(SyncError::TypeMismatch(format!(
                "{key}: expected {expected}, found {actual}"
            )))
        }
    }

    /// Strict lock order: `entry.subscribers` -> `entry.value`. Channel drops and disk writes
    /// always happen after releasing both locks.
    fn publish_typed_entry<T: ItemTrait>(
        &self,
        key: &str,
        entry: &Arc<TypedEntry<T>>,
        persist: bool,
    ) -> bool {
        let mut subs = lock_recover(&entry.subscribers);
        let serialized = {
            let val = lock_recover(&entry.value);
            debug!(key, "publishing state: {}", debug_val(&*val));
            serde_json::to_value(&*val)
        };

        let json = match serialized {
            Ok(json) => json,
            Err(err) => {
                error!(key, error = %err, "failed to serialize state for publish");
                return false;
            }
        };

        let dead = subs.publish(&json, None);
        drop(subs);
        drop(dead);

        if persist && self.should_persist(key) {
            debug!(key, "persisting to disk");
            if let Err(err) = self.persist_json(key, json) {
                error!(key, error = %err, "failed to persist state to disk");
            }
        }

        true
    }

    pub fn load<T: ItemTrait + Default>(&self, key: &str) -> T {
        let mut new_value: T = Default::default();

        if !self.should_persist(key) {
            warn!(
                key,
                "load called for key not configured to persist, returning default"
            );
            self.set::<T>(key, new_value.clone());
            return new_value;
        }

        debug!(key, "loading from disk");
        match self.disk_store() {
            Ok(store) => {
                new_value = match store.get(key) {
                    Some(val) => match serde_json::from_value(val) {
                        Ok(res) => res,
                        Err(err) => {
                            error!(
                                key,
                                error = %err,
                                "value for key did not match specified type"
                            );
                            new_value
                        }
                    },
                    None => {
                        warn!(key, "load called for key not on disk");
                        new_value
                    }
                };
            }
            Err(err) => {
                error!(key, error = %err, "failed to open disk store for load");
            }
        }

        self.set::<T>(key, new_value.clone());
        new_value
    }

    pub fn save<T: ItemTrait>(&self, key: &str) {
        if !self.should_persist(key) {
            error!(
                key,
                "save called for key not configured to persist, ignoring"
            );
            return;
        }

        let value = match self.try_snapshot::<T>(key) {
            Ok(val) => val,
            Err(err) => {
                error!(key, error = %err, "save failed to snapshot key");
                return;
            }
        };

        match serde_json::to_value(&value) {
            Ok(json) => {
                if let Err(err) = self.persist_json(key, json) {
                    error!(key, error = %err, "save failed to persist key");
                }
            }
            Err(err) => {
                error!(key, error = %err, "save failed to serialize key");
            }
        }
    }

    /// Sets the value for `key` without emitting to subscribers.
    /// When `key` already exists with the same type `T`, updates in place and preserves subscribers.
    pub fn set<T: ItemTrait>(&self, key: &str, value: T) {
        debug!(key, "set: {}", debug_val(&value));

        let existing = {
            let mut entries = lock_recover(&self.entries);
            match entries.get(key) {
                Some(entry) if entry.as_any().is::<TypedEntry<T>>() => {
                    let cloned = Arc::clone(entry);
                    let erased_any: Arc<dyn Any + Send + Sync> = cloned;
                    erased_any.downcast::<TypedEntry<T>>().ok()
                }
                Some(entry) => {
                    warn!(
                        key,
                        old_type = entry.type_name(),
                        new_type = std::any::type_name::<T>(),
                        "replacing state entry with a different type"
                    );
                    let old =
                        entries.insert(key.to_string(), Arc::new(TypedEntry::new(value.clone())));
                    drop(entries);
                    drop(old);
                    None
                }
                None => {
                    entries.insert(key.to_string(), Arc::new(TypedEntry::new(value.clone())));
                    None
                }
            }
        };

        if let Some(entry) = existing {
            let mut guard = lock_recover(&entry.value);
            *guard = value.clone();
        }

        if self.should_persist(key) {
            match serde_json::to_value(&value) {
                Ok(json) => {
                    if let Err(err) = self.persist_json(key, json) {
                        error!(key, error = %err, "failed to persist state on set");
                    }
                }
                Err(err) => {
                    error!(key, error = %err, "failed to serialize state on set");
                }
            }
        }
    }

    pub fn update<T: ItemTrait>(&self, key: &str, new_value: T, emit: bool) {
        debug!(key, emit, "update: {}", debug_val(&new_value));

        let entry = match self.get_typed_entry::<T>(key) {
            Ok(entry) => entry,
            Err(SyncError::UnknownKey(_)) => {
                info!(
                    key,
                    "updating a key that doesn't exist yet, setting it instead"
                );
                self.set(key, new_value);
                return;
            }
            Err(err) => {
                error!(key, error = %err, "failed to update key");
                return;
            }
        };

        let persist = self.should_persist(key);

        if emit {
            let mut subs = lock_recover(&entry.subscribers);
            let serialized = {
                let mut val = lock_recover(&entry.value);
                *val = new_value;
                serde_json::to_value(&*val)
            };

            let json = match serialized {
                Ok(json) => json,
                Err(err) => {
                    error!(key, error = %err, "failed to serialize state on update");
                    return;
                }
            };

            let dead = subs.publish(&json, None);
            drop(subs);
            drop(dead);

            if persist && let Err(err) = self.persist_json(key, json) {
                error!(key, error = %err, "failed to persist state on update");
            }
        } else {
            let serialized = {
                let mut val = lock_recover(&entry.value);
                *val = new_value;
                if persist {
                    Some(serde_json::to_value(&*val))
                } else {
                    None
                }
            };

            if let Some(serialized) = serialized {
                match serialized {
                    Ok(json) => {
                        if let Err(err) = self.persist_json(key, json) {
                            error!(key, error = %err, "failed to persist state on update");
                        }
                    }
                    Err(err) => {
                        error!(key, error = %err, "failed to serialize state on update");
                    }
                }
            }
        }
    }

    pub fn try_get<T: ItemTrait>(&self, key: &str) -> Result<Item<T, R>, SyncError> {
        debug!(key, "try_get");
        let entry = self.get_typed_entry::<T>(key)?;
        Ok(Item {
            entry,
            key: key.to_string(),
            syncer: self.clone(),
        })
    }

    pub fn get<T: ItemTrait>(&self, key: &str) -> Item<T, R> {
        self.try_get::<T>(key)
            .unwrap_or_else(|err| panic!("StateSyncer::get failed: {err}"))
    }

    pub fn try_snapshot<T: ItemTrait>(&self, key: &str) -> Result<T, SyncError> {
        debug!(key, "try_snapshot");
        let entry = self.get_typed_entry::<T>(key)?;
        let guard = lock_recover(&entry.value);
        Ok(guard.clone())
    }

    pub fn snapshot<T: ItemTrait>(&self, key: &str) -> T {
        self.try_snapshot::<T>(key)
            .unwrap_or_else(|err| panic!("StateSyncer::snapshot failed: {err}"))
    }

    pub fn emit<T: ItemTrait>(&self, name: &str) -> bool {
        debug!(key = name, "emit");
        let entry = match self.get_typed_entry::<T>(name) {
            Ok(entry) => entry,
            Err(err) => {
                warn!(key = name, error = %err, "emit called for invalid key");
                return false;
            }
        };

        self.publish_typed_entry(name, &entry, false)
    }

    pub fn subscribe(
        &self,
        name: &str,
        webview: &Webview<R>,
        channel: Channel<SyncMessage>,
    ) -> Result<String, SyncError> {
        let webview_label = webview.label().to_string();
        let window_label = webview.window().label().to_string();
        self.subscribe_with_labels(name, webview_label, window_label, channel)
    }

    pub fn subscribe_with_labels(
        &self,
        name: &str,
        webview_label: impl Into<String>,
        window_label: impl Into<String>,
        channel: Channel<SyncMessage>,
    ) -> Result<String, SyncError> {
        let webview_label = webview_label.into();
        let window_label = window_label.into();
        let entry = self.get_erased_entry(name)?;

        let mut subs = lock_recover(entry.subscribers());
        let snapshot = entry.snapshot_json()?;

        if let Err(err) = channel.send(SyncMessage::Snapshot { value: snapshot }) {
            drop(subs);
            drop(channel);
            return Err(SyncError::ChannelSendFailed(err.to_string()));
        }

        let id = format!(
            "sub-{}",
            self.next_subscriber_id.fetch_add(1, Ordering::Relaxed)
        );
        debug!(
            key = name,
            subscriber_id = %id,
            webview = %webview_label,
            window = %window_label,
            "registered channel subscriber"
        );
        subs.subscribers.push(Subscriber {
            id: id.clone(),
            webview_label,
            window_label,
            channel,
        });

        Ok(id)
    }

    pub fn unsubscribe(
        &self,
        name: &str,
        webview_label: &str,
        subscriber_id: &str,
    ) -> Result<bool, SyncError> {
        let entry = self.get_erased_entry(name)?;
        let removed = {
            let mut subs = lock_recover(entry.subscribers());
            subs.remove_by_id(webview_label, subscriber_id)
        };
        let was_removed = removed.is_some();
        drop(removed);
        Ok(was_removed)
    }

    pub fn update_from_frontend(&self, update: StateUpdate) -> Result<(), SyncError> {
        debug!(
            key = %update.name,
            subscriber_id = ?update.subscriber_id,
            "update_from_frontend"
        );

        let entry = self.get_erased_entry(&update.name)?;
        let mut subs = lock_recover(entry.subscribers());
        let committed = entry.update_from_json(update.value)?;
        let dead = subs.publish(&committed, update.subscriber_id.as_deref());
        drop(subs);
        drop(dead);

        if self.should_persist(&update.name) {
            self.persist_json(&update.name, committed)?;
        }

        Ok(())
    }

    pub fn clear_webview(&self, webview_label: &str) -> usize {
        let entries: Vec<_> = {
            let guard = lock_recover(&self.entries);
            guard.values().cloned().collect()
        };

        let mut removed = Vec::new();
        for entry in entries {
            let mut subs = lock_recover(entry.subscribers());
            removed.extend(subs.remove_by_webview(webview_label));
        }

        let count = removed.len();
        drop(removed);
        count
    }

    pub fn clear_window(&self, window_label: &str) -> usize {
        let entries: Vec<_> = {
            let guard = lock_recover(&self.entries);
            guard.values().cloned().collect()
        };

        let mut removed = Vec::new();
        for entry in entries {
            let mut subs = lock_recover(entry.subscribers());
            removed.extend(subs.remove_by_window(window_label));
        }

        let count = removed.len();
        drop(removed);
        count
    }

    pub fn subscriber_count(&self, name: &str) -> usize {
        let Ok(entry) = self.get_erased_entry(name) else {
            return 0;
        };
        let subs = lock_recover(entry.subscribers());
        subs.subscribers.len()
    }
}

/// Generates local `subscribe_state`, `unsubscribe_state`, and `update_state` Tauri + Specta
/// commands backed by `StateSyncer`.
#[macro_export]
macro_rules! state_handlers {
    () => {
        #[tauri::command]
        #[specta::specta]
        fn subscribe_state(
            name: String,
            channel: tauri::ipc::Channel<$crate::SyncMessage>,
            webview: tauri::Webview<tauri::Wry>,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<String, $crate::SyncError> {
            state_syncer.subscribe(&name, &webview, channel)
        }

        #[tauri::command]
        #[specta::specta]
        fn unsubscribe_state(
            name: String,
            subscriber_id: String,
            webview: tauri::Webview<tauri::Wry>,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<bool, $crate::SyncError> {
            state_syncer.unsubscribe(&name, webview.label(), &subscriber_id)
        }

        #[tauri::command]
        #[specta::specta]
        fn update_state(
            state: $crate::StateUpdate,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<bool, $crate::SyncError> {
            state_syncer.update_from_frontend(state)?;
            Ok(true)
        }
    };
}
