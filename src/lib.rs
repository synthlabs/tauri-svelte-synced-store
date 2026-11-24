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
mod patch;
mod plugin;
mod protocol;

pub use plugin::plugin;
pub use protocol::{
    DeliveryMode, JsonPatchOp, JsonValue, StateSnapshot, StateUpdate, SyncError, SyncMessage,
    UpdateOutcome, UpdatePayload,
};

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
    mode: DeliveryMode,
    last_sent_revision: u64,
    channel: Channel<SyncMessage>,
}

struct EntrySyncState {
    revision: u64,
    current_json: serde_json::Value,
    current_json_bytes: usize,
    subscribers: Vec<Subscriber>,
}

impl EntrySyncState {
    fn new(current_json: serde_json::Value, current_json_bytes: usize) -> Self {
        Self {
            revision: 0,
            current_json,
            current_json_bytes,
            subscribers: Vec::new(),
        }
    }

    fn has_delta_eligible_subscriber(
        &self,
        key_mode: DeliveryMode,
        exclude_subscriber_id: Option<&str>,
    ) -> bool {
        if key_mode == DeliveryMode::Snapshot {
            return false;
        }
        self.subscribers.iter().any(|sub| {
            exclude_subscriber_id != Some(sub.id.as_str())
                && sub.mode == DeliveryMode::Delta
                && sub.last_sent_revision == self.revision
        })
    }

    /// Publishes a transition from `prev_revision` to `self.revision` to all subscribers except
    /// `exclude_subscriber_id`. Returns failed subscribers to drop outside the lock.
    fn publish_transition(
        &mut self,
        prev_revision: u64,
        patch_ops: Option<&[JsonPatchOp]>,
        exclude_subscriber_id: Option<&str>,
    ) -> Vec<Subscriber> {
        let use_delta = match patch_ops {
            Some(ops) if !ops.is_empty() => patch::json_byte_len(&ops)
                .is_ok_and(|patch_bytes| patch_bytes < self.current_json_bytes),
            _ => false,
        };

        let mut dead = Vec::new();
        let mut idx = 0;

        while idx < self.subscribers.len() {
            if exclude_subscriber_id == Some(self.subscribers[idx].id.as_str()) {
                self.subscribers[idx].last_sent_revision = self.revision;
                idx += 1;
                continue;
            }

            let msg = if use_delta
                && self.subscribers[idx].mode == DeliveryMode::Delta
                && self.subscribers[idx].last_sent_revision == prev_revision
                && let Some(ops) = patch_ops
            {
                SyncMessage::Delta {
                    base_revision: prev_revision,
                    revision: self.revision,
                    patch: ops.to_vec(),
                }
            } else {
                SyncMessage::Snapshot {
                    revision: self.revision,
                    value: self.current_json.clone(),
                }
            };

            match self.subscribers[idx].channel.send(msg) {
                Ok(()) => {
                    self.subscribers[idx].last_sent_revision = self.revision;
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

    fn publish_snapshot_all(&mut self) -> Vec<Subscriber> {
        let mut dead = Vec::new();
        let mut idx = 0;

        while idx < self.subscribers.len() {
            let msg = SyncMessage::Snapshot {
                revision: self.revision,
                value: self.current_json.clone(),
            };
            match self.subscribers[idx].channel.send(msg) {
                Ok(()) => {
                    self.subscribers[idx].last_sent_revision = self.revision;
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

    fn mark_subscriber_revision(
        &mut self,
        webview_label: Option<&str>,
        subscriber_id: &str,
        revision: u64,
    ) {
        if let Some(sub) = self.subscribers.iter_mut().find(|sub| {
            sub.id == subscriber_id
                && webview_label.is_none_or(|webview| sub.webview_label == webview)
        }) {
            sub.last_sent_revision = revision;
        }
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
    sync_state: Mutex<EntrySyncState>,
}

impl<T: ItemTrait> TypedEntry<T> {
    fn try_new(value: T) -> Result<Self, SyncError> {
        let current_json = serde_json::to_value(&value)
            .map_err(|err| SyncError::SerializeFailed(err.to_string()))?;
        let current_json_bytes = patch::json_byte_len(&current_json)?;
        Ok(Self {
            value: Mutex::new(value),
            sync_state: Mutex::new(EntrySyncState::new(current_json, current_json_bytes)),
        })
    }
}

struct CommittedCandidate {
    canonical: serde_json::Value,
    normalized: bool,
}

trait ErasedEntry: Any + Send + Sync {
    fn as_any(&self) -> &(dyn Any + Send + Sync);
    fn sync_state(&self) -> &Mutex<EntrySyncState>;
    fn apply_candidate(
        &self,
        candidate: &serde_json::Value,
        current_json: &serde_json::Value,
    ) -> Result<Option<CommittedCandidate>, SyncError>;
    fn type_name(&self) -> &'static str;
}

impl<T: ItemTrait> ErasedEntry for TypedEntry<T> {
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn sync_state(&self) -> &Mutex<EntrySyncState> {
        &self.sync_state
    }

    fn apply_candidate(
        &self,
        candidate: &serde_json::Value,
        current_json: &serde_json::Value,
    ) -> Result<Option<CommittedCandidate>, SyncError> {
        let parsed =
            T::deserialize(candidate).map_err(|err| SyncError::InvalidValue(err.to_string()))?;
        let canonical = serde_json::to_value(&parsed)
            .map_err(|err| SyncError::SerializeFailed(err.to_string()))?;
        if canonical == *current_json {
            return Ok(None);
        }

        let normalized = canonical != *candidate;
        let mut guard = lock_recover(&self.value);
        *guard = parsed;
        Ok(Some(CommittedCandidate {
            canonical,
            normalized,
        }))
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

/// Guard handle returned by `StateSyncer::get` that publishes and persists state on drop
/// whenever the serialized value changed.
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
    pub delivery_modes: HashMap<String, DeliveryMode>,
    pub default_delivery_mode: DeliveryMode,
}

impl Default for StateSyncerConfig {
    fn default() -> Self {
        Self {
            filename: "state.json".to_owned(),
            persist_keys: HashMap::new(),
            default_persist: false,
            delivery_modes: HashMap::new(),
            default_delivery_mode: DeliveryMode::Delta,
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

    fn key_delivery_mode(&self, key: &str) -> DeliveryMode {
        *self
            .cfg
            .delivery_modes
            .get(key)
            .unwrap_or(&self.cfg.default_delivery_mode)
    }

    fn effective_delivery_mode(&self, key: &str, requested: DeliveryMode) -> DeliveryMode {
        if self.key_delivery_mode(key) == DeliveryMode::Snapshot
            || requested == DeliveryMode::Snapshot
        {
            DeliveryMode::Snapshot
        } else {
            DeliveryMode::Delta
        }
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

    /// Applies `new_json` to `sync_state`, skipping work if unchanged, and optionally broadcasts
    /// to subscribers. Caller must hold `sync_state` (and no `value` lock); returns dead
    /// subscribers and optional JSON to persist outside all locks.
    fn commit_json_transition(
        &self,
        key: &str,
        sync_state: &mut EntrySyncState,
        new_json: serde_json::Value,
        emit: bool,
        persist: bool,
    ) -> Result<(Vec<Subscriber>, Option<serde_json::Value>), SyncError> {
        let key_mode = self.key_delivery_mode(key);
        let patch_ops = if emit && sync_state.has_delta_eligible_subscriber(key_mode, None) {
            let ops = patch::diff_json(&sync_state.current_json, &new_json);
            if ops.is_empty() {
                return Ok((Vec::new(), None));
            }
            Some(ops)
        } else {
            if new_json == sync_state.current_json {
                return Ok((Vec::new(), None));
            }
            None
        };

        let new_json_bytes = patch::json_byte_len(&new_json)?;
        let prev_revision = sync_state.revision;
        sync_state.revision = prev_revision + 1;
        sync_state.current_json = new_json;
        sync_state.current_json_bytes = new_json_bytes;

        let dead = if emit {
            sync_state.publish_transition(prev_revision, patch_ops.as_deref(), None)
        } else {
            Vec::new()
        };

        let persist_val = if persist && self.should_persist(key) {
            Some(sync_state.current_json.clone())
        } else {
            None
        };

        Ok((dead, persist_val))
    }

    /// Strict lock order: `entry.sync_state` -> `entry.value`. Channel drops and disk writes
    /// always happen after releasing both locks.
    fn publish_typed_entry<T: ItemTrait>(
        &self,
        key: &str,
        entry: &Arc<TypedEntry<T>>,
        persist: bool,
    ) -> bool {
        let mut sync_state = lock_recover(&entry.sync_state);
        let serialized = {
            let val = lock_recover(&entry.value);
            debug!(key, "publishing state: {}", debug_val(&*val));
            serde_json::to_value(&*val)
        };

        let new_json = match serialized {
            Ok(json) => json,
            Err(err) => {
                error!(key, error = %err, "failed to serialize state for publish");
                return false;
            }
        };

        let (dead, persist_val) =
            match self.commit_json_transition(key, &mut sync_state, new_json, true, persist) {
                Ok(res) => res,
                Err(err) => {
                    error!(key, error = %err, "failed to prepare state publish");
                    return false;
                }
            };
        drop(sync_state);
        drop(dead);

        if let Some(json) = persist_val {
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
    /// When `key` already exists with the same type `T`, updates in place, bumps revision if
    /// serialized state changed, and preserves subscribers.
    pub fn set<T: ItemTrait>(&self, key: &str, value: T) {
        debug!(key, "set: {}", debug_val(&value));

        let mut created_json: Option<serde_json::Value> = None;
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
                    let new_entry = match TypedEntry::try_new(value.clone()) {
                        Ok(entry) => entry,
                        Err(err) => {
                            error!(key, error = %err, "failed to serialize state on set");
                            return;
                        }
                    };
                    if self.should_persist(key) {
                        created_json =
                            Some(lock_recover(&new_entry.sync_state).current_json.clone());
                    }
                    let old = entries.insert(key.to_string(), Arc::new(new_entry));
                    drop(entries);
                    drop(old);
                    None
                }
                None => {
                    let new_entry = match TypedEntry::try_new(value.clone()) {
                        Ok(entry) => entry,
                        Err(err) => {
                            error!(key, error = %err, "failed to serialize state on set");
                            return;
                        }
                    };
                    if self.should_persist(key) {
                        created_json =
                            Some(lock_recover(&new_entry.sync_state).current_json.clone());
                    }
                    entries.insert(key.to_string(), Arc::new(new_entry));
                    None
                }
            }
        };

        let persist_val = if let Some(entry) = existing {
            let mut sync_state = lock_recover(&entry.sync_state);
            let serialized = {
                let mut guard = lock_recover(&entry.value);
                *guard = value;
                serde_json::to_value(&*guard)
            };

            let new_json = match serialized {
                Ok(json) => json,
                Err(err) => {
                    error!(key, error = %err, "failed to serialize state on set");
                    return;
                }
            };

            let (dead, persist_val) =
                match self.commit_json_transition(key, &mut sync_state, new_json, false, true) {
                    Ok(res) => res,
                    Err(err) => {
                        error!(key, error = %err, "failed to update state revision on set");
                        return;
                    }
                };
            drop(sync_state);
            drop(dead);
            persist_val
        } else {
            created_json
        };

        if let Some(json) = persist_val
            && let Err(err) = self.persist_json(key, json)
        {
            error!(key, error = %err, "failed to persist state on set");
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

        let mut sync_state = lock_recover(&entry.sync_state);
        let serialized = {
            let mut val = lock_recover(&entry.value);
            *val = new_value;
            serde_json::to_value(&*val)
        };

        let new_json = match serialized {
            Ok(json) => json,
            Err(err) => {
                error!(key, error = %err, "failed to serialize state on update");
                return;
            }
        };

        let (dead, persist_val) =
            match self.commit_json_transition(key, &mut sync_state, new_json, emit, true) {
                Ok(res) => res,
                Err(err) => {
                    error!(key, error = %err, "failed to prepare state update");
                    return;
                }
            };
        drop(sync_state);
        drop(dead);

        if let Some(json) = persist_val
            && let Err(err) = self.persist_json(key, json)
        {
            error!(key, error = %err, "failed to persist state on update");
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

    pub fn revision(&self, name: &str) -> Result<u64, SyncError> {
        let entry = self.get_erased_entry(name)?;
        let sync_state = lock_recover(entry.sync_state());
        Ok(sync_state.revision)
    }

    /// Forces a full snapshot broadcast to all subscribers of `name`.
    pub fn emit<T: ItemTrait>(&self, name: &str) -> bool {
        debug!(key = name, "emit");
        let entry = match self.get_typed_entry::<T>(name) {
            Ok(entry) => entry,
            Err(err) => {
                warn!(key = name, error = %err, "emit called for invalid key");
                return false;
            }
        };

        let mut sync_state = lock_recover(&entry.sync_state);
        let serialized = {
            let val = lock_recover(&entry.value);
            serde_json::to_value(&*val)
        };

        let live_json = match serialized {
            Ok(json) => json,
            Err(err) => {
                error!(key = name, error = %err, "failed to serialize state for emit");
                return false;
            }
        };

        if live_json != sync_state.current_json {
            let Ok(bytes) = patch::json_byte_len(&live_json) else {
                error!(key = name, "failed to measure serialized state for emit");
                return false;
            };
            sync_state.revision += 1;
            sync_state.current_json = live_json;
            sync_state.current_json_bytes = bytes;
        }

        let dead = sync_state.publish_snapshot_all();
        drop(sync_state);
        drop(dead);
        true
    }

    pub fn subscribe(
        &self,
        name: &str,
        webview: &Webview<R>,
        channel: Channel<SyncMessage>,
        mode: Option<DeliveryMode>,
    ) -> Result<String, SyncError> {
        let webview_label = webview.label().to_string();
        let window_label = webview.window().label().to_string();
        self.subscribe_with_labels(
            name,
            webview_label,
            window_label,
            channel,
            mode.unwrap_or_default(),
        )
    }

    pub fn subscribe_with_labels(
        &self,
        name: &str,
        webview_label: impl Into<String>,
        window_label: impl Into<String>,
        channel: Channel<SyncMessage>,
        mode: DeliveryMode,
    ) -> Result<String, SyncError> {
        let webview_label = webview_label.into();
        let window_label = window_label.into();
        let entry = self.get_erased_entry(name)?;
        let effective_mode = self.effective_delivery_mode(name, mode);

        let mut sync_state = lock_recover(entry.sync_state());
        let initial_msg = SyncMessage::Snapshot {
            revision: sync_state.revision,
            value: sync_state.current_json.clone(),
        };

        if let Err(err) = channel.send(initial_msg) {
            drop(sync_state);
            drop(channel);
            return Err(SyncError::ChannelSendFailed(err.to_string()));
        }

        let id = format!(
            "sub-{}",
            self.next_subscriber_id.fetch_add(1, Ordering::Relaxed)
        );
        let revision = sync_state.revision;
        debug!(
            key = name,
            subscriber_id = %id,
            webview = %webview_label,
            window = %window_label,
            mode = ?effective_mode,
            revision,
            "registered channel subscriber"
        );
        sync_state.subscribers.push(Subscriber {
            id: id.clone(),
            webview_label,
            window_label,
            mode: effective_mode,
            last_sent_revision: revision,
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
            let mut sync_state = lock_recover(entry.sync_state());
            sync_state.remove_by_id(webview_label, subscriber_id)
        };
        let was_removed = removed.is_some();
        drop(removed);
        Ok(was_removed)
    }

    pub fn sync_snapshot(
        &self,
        name: &str,
        webview_label: Option<&str>,
        subscriber_id: Option<&str>,
    ) -> Result<StateSnapshot, SyncError> {
        let entry = self.get_erased_entry(name)?;
        let mut sync_state = lock_recover(entry.sync_state());
        let revision = sync_state.revision;
        if let Some(sub_id) = subscriber_id {
            sync_state.mark_subscriber_revision(webview_label, sub_id, revision);
        }
        Ok(StateSnapshot {
            revision,
            value: sync_state.current_json.clone(),
        })
    }

    pub fn update_from_frontend(&self, update: StateUpdate) -> Result<UpdateOutcome, SyncError> {
        debug!(
            key = %update.name,
            base_revision = update.base_revision,
            subscriber_id = ?update.subscriber_id,
            "update_from_frontend"
        );

        let entry = self.get_erased_entry(&update.name)?;
        let key_mode = self.key_delivery_mode(&update.name);
        let mut sync_state = lock_recover(entry.sync_state());

        if update.base_revision != sync_state.revision {
            let revision = sync_state.revision;
            if let Some(sub_id) = update.subscriber_id.as_deref() {
                sync_state.mark_subscriber_revision(None, sub_id, revision);
            }
            return Ok(UpdateOutcome::Stale {
                snapshot: StateSnapshot {
                    revision,
                    value: sync_state.current_json.clone(),
                },
            });
        }

        let (candidate, submitted_patch) = match update.payload {
            UpdatePayload::Full { value } => (value, None),
            UpdatePayload::Delta { patch: ops } => {
                if ops.is_empty() {
                    return Ok(UpdateOutcome::Unchanged {
                        revision: sync_state.revision,
                    });
                }
                let applied = patch::apply_patch(&sync_state.current_json, &ops)?;
                (applied, Some(ops))
            }
        };

        let Some(committed) = entry.apply_candidate(&candidate, &sync_state.current_json)? else {
            return Ok(UpdateOutcome::Unchanged {
                revision: sync_state.revision,
            });
        };

        let new_json_bytes = patch::json_byte_len(&committed.canonical)?;
        let exclude = update.subscriber_id.as_deref();
        let fanout_patch = if sync_state.has_delta_eligible_subscriber(key_mode, exclude) {
            match submitted_patch {
                Some(ops) if !committed.normalized => Some(ops),
                _ => Some(patch::diff_json(
                    &sync_state.current_json,
                    &committed.canonical,
                )),
            }
        } else {
            None
        };

        let prev_revision = sync_state.revision;
        let next_revision = prev_revision + 1;
        sync_state.revision = next_revision;
        sync_state.current_json = committed.canonical;
        sync_state.current_json_bytes = new_json_bytes;

        let dead = sync_state.publish_transition(prev_revision, fanout_patch.as_deref(), exclude);
        let persist_val = if self.should_persist(&update.name) {
            Some(sync_state.current_json.clone())
        } else {
            None
        };
        let canonical_out = if committed.normalized {
            Some(sync_state.current_json.clone())
        } else {
            None
        };
        drop(sync_state);
        drop(dead);

        if let Some(json) = persist_val {
            self.persist_json(&update.name, json)?;
        }

        Ok(UpdateOutcome::Synced {
            revision: next_revision,
            canonical: canonical_out,
        })
    }

    pub fn clear_webview(&self, webview_label: &str) -> usize {
        let entries: Vec<_> = {
            let guard = lock_recover(&self.entries);
            guard.values().cloned().collect()
        };

        let mut removed = Vec::new();
        for entry in entries {
            let mut sync_state = lock_recover(entry.sync_state());
            removed.extend(sync_state.remove_by_webview(webview_label));
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
            let mut sync_state = lock_recover(entry.sync_state());
            removed.extend(sync_state.remove_by_window(window_label));
        }

        let count = removed.len();
        drop(removed);
        count
    }

    pub fn subscriber_count(&self, name: &str) -> usize {
        let Ok(entry) = self.get_erased_entry(name) else {
            return 0;
        };
        let sync_state = lock_recover(entry.sync_state());
        sync_state.subscribers.len()
    }
}

/// Generates local `subscribe_state`, `unsubscribe_state`, `snapshot_state`, and `update_state`
/// Tauri + Specta commands backed by `StateSyncer`.
#[macro_export]
macro_rules! state_handlers {
    () => {
        #[tauri::command]
        #[specta::specta]
        fn subscribe_state(
            name: String,
            channel: tauri::ipc::Channel<$crate::SyncMessage>,
            mode: Option<$crate::DeliveryMode>,
            webview: tauri::Webview<tauri::Wry>,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<String, $crate::SyncError> {
            state_syncer.subscribe(&name, &webview, channel, mode)
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
        fn snapshot_state(
            name: String,
            subscriber_id: Option<String>,
            webview: tauri::Webview<tauri::Wry>,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<$crate::StateSnapshot, $crate::SyncError> {
            state_syncer.sync_snapshot(&name, Some(webview.label()), subscriber_id.as_deref())
        }

        #[tauri::command]
        #[specta::specta]
        fn update_state(
            state: $crate::StateUpdate,
            state_syncer: tauri::State<'_, $crate::StateSyncer>,
        ) -> Result<$crate::UpdateOutcome, $crate::SyncError> {
            state_syncer.update_from_frontend(state)
        }
    };
}
