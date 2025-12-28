use serde::de::{DeserializeOwned, Error};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fmt::Debug;
use std::sync::{LockResult, MutexGuard};
use std::{
    any::Any,
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter};
use tauri_plugin_store::{Store, StoreExt};
use tauri_specta::Event;
use tracing::{debug, error, info, warn};

#[derive(Deserialize, Serialize, Type, Clone, Debug, Event)]
pub struct StateUpdate {
    pub version: Option<u128>,
    pub name: String,
    pub value: String,
}

// Define an alias trait that combines all the required traits
pub trait ItemTrait: 'static + Send + Sync + Serialize + DeserializeOwned + Debug + Clone {}
// Blanket impl
impl<'r, T> ItemTrait for T where
    T: 'static + Send + Sync + Serialize + DeserializeOwned + Debug + Clone
{
}

// Item wraps an object and emits an update of the wrapped object when Item is dropped
// the object is expected to be wrapped in a mutex
pub struct Item<'r, T: ItemTrait>(
    &'r Mutex<T>,               // 0: value
    &'r str,                    // 1: key
    &'r AppHandle,              // 2: tauri app ref
    &'r bool,                   // 3: save_to_disk
    &'r Arc<Store<tauri::Wry>>, // 4: disk_store
);

impl<'r, T: ItemTrait> Item<'r, T> {
    pub fn lock(&'_ self) -> LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }
}

impl<'r, T: ItemTrait> Drop for Item<'r, T> {
    fn drop(&mut self) {
        let self_guard = self.0.lock().unwrap();
        debug!("[Item] dropped: {:?}", *self_guard);

        let name = format!("{}_update", self.1);
        self.2
            .emit(&name, self_guard.clone())
            .expect("unable to emit state");

        // if disk persist is enabled
        if *self.3 {
            debug!("[Item] persisting to disk: {}", self.1);
            self.4.set(self.1, serde_json::json!(*self_guard));
        }
    }
}

impl<'r, T: ItemTrait> Clone for Item<'r, T> {
    fn clone(&self) -> Self {
        Item(self.0, self.1, self.2, self.3, self.4)
    }
}

impl<'r, T: ItemTrait + PartialEq> PartialEq for Item<'r, T> {
    fn eq(&self, other: &Self) -> bool {
        let self_guard = self.0.lock().unwrap();
        let other_guard = other.0.lock().unwrap();
        self_guard.eq(&other_guard) && self.1 == other.1
    }
}

struct Serializers {
    _from_str: Box<dyn Fn(&str) -> Result<Box<dyn Any + Send>, serde_json::Error> + Send>,
    _to_str: Box<dyn Fn(&dyn Any) -> Result<String, serde_json::Error> + Send>,
}

type MapAny = HashMap<String, Pin<Box<dyn Any + Send + Sync>>>;
type SerializersMap = HashMap<String, Serializers>;

#[derive(Clone)]
pub struct StateSyncerConfig {
    pub sync_to_disk: bool,
    pub filename: String,
}

impl Default for StateSyncerConfig {
    fn default() -> Self {
        Self {
            sync_to_disk: false,
            filename: "state.json".to_owned(),
        }
    }
}

#[derive(Clone)]
pub struct StateSyncer {
    data: Arc<Mutex<MapAny>>,
    serializers: Arc<Mutex<SerializersMap>>,
    app: AppHandle,
    cfg: StateSyncerConfig,
    disk_store: Arc<Store<tauri::Wry>>,
}

impl StateSyncer {
    pub fn new(cfg: StateSyncerConfig, app: AppHandle) -> Self {
        let syncer = StateSyncer {
            data: Default::default(),
            serializers: Default::default(),
            app: app.clone(),
            cfg: cfg.clone(),
            disk_store: app.store(cfg.filename).unwrap(),
        };

        syncer
    }

    pub fn load<'a, T: ItemTrait + std::default::Default>(&self, key: &str) -> T {
        let mut new_value: T = Default::default();

        if !self.cfg.sync_to_disk {
            warn!(
                key,
                "load called with sync_to_disk disabled, returning default"
            );
            self.set::<T>(key, new_value.clone());
            return new_value;
        }

        debug!(key, "loading from disk");
        new_value = match self.disk_store.get(key) {
            Some(val) => match serde_json::from_value(val) {
                Ok(res) => res,
                Err(_) => {
                    error!(key, "value for key did not match specified type");
                    new_value
                }
            },
            None => {
                warn!(key, "load called for key not on disk");
                new_value
            }
        };

        self.set::<T>(key, new_value.clone());

        new_value
    }

    pub fn save<'a, T: ItemTrait>(&self, key: &str) {
        if !self.cfg.sync_to_disk {
            error!("save called with sync_to_disk disabled, ignoring");
            return;
        }
        let value = self.snapshot::<T>(key);

        self.persist(key, value);
    }

    fn persist<'a, T: ItemTrait>(&self, key: &str, value: T) {
        self.disk_store.set(key, serde_json::json!(value));
    }

    pub fn update_typed_string<'a, T: ItemTrait>(&self, key: &str, value: &'a str, emit: bool) {
        debug!(key, "update_typed_string");
        let new_value: T = match serde_json::from_str(value) {
            Ok(res) => res,
            Err(_) => {
                error!("failed to parse internal state");
                return;
            }
        };

        self.update(key, new_value, emit);
    }

    pub fn update<'a, T: ItemTrait>(&self, key: &str, new_value: T, emit: bool) {
        debug!(key, "update: {:?}", new_value);
        let key_exists: bool;
        {
            let guard = self.data.lock().unwrap();
            key_exists = guard.contains_key(key);
        }
        if !key_exists {
            info!("updating a key that doesn't exist yet, setting it instead");
            self.set(key, new_value);
            return;
        }

        let guard = self.data.lock().unwrap();
        let ptr = guard.get(key).unwrap();
        let value = unsafe {
            ptr.downcast_ref::<Mutex<T>>()
                // SAFETY: the type of the key is the same as the type of the value
                .unwrap_unchecked()
        };
        let v_ref = unsafe { &*(value as *const Mutex<T>) };

        let mut v_guard = v_ref.lock().unwrap();
        *v_guard = new_value.clone();

        if self.cfg.sync_to_disk {
            self.persist(key, new_value.clone());
        }

        if emit {
            let key = format!("{}_update", key);
            debug!("emitting {}: {:?}", key, new_value.clone());
            self.app
                .emit(key.as_str(), new_value.clone())
                .expect("unable to emit state");
        }
    }

    pub fn set<'a, T: ItemTrait>(&self, key: &str, value: T) {
        debug!(key, "set: {:?}", value);

        {
            let mut ds_guard = self.serializers.lock().unwrap();
            if !ds_guard.contains_key(key) {
                debug!(key, "no serializers set for this key yet, adding it");
                let deserializer =
                    move |s: &str| -> Result<Box<dyn Any + Send>, serde_json::Error> {
                        debug!(type = std::any::type_name:: <T>(), "deserializing");
                        let value: T = serde_json::from_str(s)?;
                        Ok(Box::new(value))
                    };

                let serializer = move |obj: &dyn Any| {
                    debug!(real_type = std::any::type_name::<T>(), "serializing");

                    if let Some(concrete) = obj.downcast_ref::<T>() {
                        serde_json::to_string::<T>(concrete)
                    } else {
                        Err(serde_json::Error::custom("Type mismatch"))
                    }
                };

                let s = Serializers {
                    _from_str: Box::new(deserializer),
                    _to_str: Box::new(serializer),
                };

                ds_guard.insert(key.to_string(), s);
            }
        }

        let mut map_guard = self.data.lock().unwrap();
        map_guard.insert(key.to_string(), Box::pin(Mutex::new(value.clone())));
        if self.cfg.sync_to_disk {
            self.persist(key, value.clone());
        }
    }

    // get a mutex protexted item that will emit an update event when dropped
    pub fn get<'a, T: ItemTrait>(&'a self, key: &'a str) -> Item<'a, T> {
        debug!(key, "get");
        let guard = self.data.lock().unwrap();
        let ptr = guard.get(key).unwrap();
        let value = unsafe {
            ptr.downcast_ref::<Mutex<T>>()
                // SAFETY: the type of the key is the same as the type of the value
                .unwrap_unchecked()
        };
        let v_ref = unsafe { &*(value as *const Mutex<T>) };

        Item(
            v_ref,
            key,
            &self.app,
            &self.cfg.sync_to_disk,
            &self.disk_store,
        )
    }

    // snapshot an Item in the cache as a read-only reference of the current state
    pub fn snapshot<'a, T: ItemTrait>(&'a self, key: &'a str) -> T {
        debug!(key, "get");
        let guard = self.data.lock().unwrap();
        let ptr = guard.get(key).unwrap();
        let value = unsafe {
            ptr.downcast_ref::<Mutex<T>>()
                // SAFETY: the type of the key is the same as the type of the value
                .unwrap_unchecked()
        };
        let v_ref = unsafe { &*(value as *const Mutex<T>) };
        let guard = v_ref.lock().unwrap();

        guard.clone()
    }

    // emit an update even for the current item's state
    pub fn emit<'a, T: ItemTrait>(&self, name: &str) -> bool {
        debug!(key = name, "emit");
        let guard = self.data.lock().unwrap();
        let ptr = guard.get(name).unwrap();
        let value = unsafe {
            ptr.downcast_ref::<Mutex<T>>()
                // SAFETY: the type of the key is the same as the type of the value
                .unwrap_unchecked()
        };
        let v_ref = unsafe { &*(value as *const Mutex<T>) };
        let value: MutexGuard<'_, T> = match v_ref.lock() {
            Ok(val) => val,
            Err(_) => return false,
        };

        let key = format!("{}_update", name);
        debug!("emitting {}: {:?}", name, value.clone());
        self.app
            .emit(key.as_str(), value.clone())
            .expect("unable to emit state");
        return true;
    }
}

#[macro_export]
macro_rules! state_handlers {
    ($($state_type:ident = $state_name:expr),* $(,)?) => {
        #[tauri::command]
        #[specta::specta]
        fn emit_state(name: String, state_syncer: tauri::State<'_, tauri_svelte_synced_store::StateSyncer>) -> bool {
            tracing::info!("emit_state: {:?}", name);

            match name.as_str() {
                $(
                    $state_name => state_syncer.emit::<$state_type>($state_name),
                )*
                _ => return false,
            }
        }

        #[tauri::command]
        #[specta::specta]
        fn update_state(state: tauri_svelte_synced_store::StateUpdate, state_syncer: tauri::State<'_, tauri_svelte_synced_store::StateSyncer>) -> bool {
            tracing::info!("update_state: {:?}", state);

            match state.name.as_str() {
                $(
                    $state_name => {
                        state_syncer
                            .update_typed_string::<$state_type>($state_name, state.value.as_str(), true);
                    }
                )*
                _ => {
                    tracing::warn!("unknown type")
                }
            }
            return true;
        }
    };
}

#[macro_export]
macro_rules! state_listener {
    ($app:expr, $syncer:expr, $($state_type:ident = $state_name:expr),* $(,)?) => {
        StateUpdate::listen(&$app, move |event| {
            warn!("state update handler: {:?}", event.payload);

            match event.payload.name.as_str() {
                $(
                    $state_name => {
                        $syncer.update_typed_string::<$state_type>(
                            $state_name,
                            event.payload.value.as_str(),
                            false,
                        );
                    }
                )*
                _ => return,
            }
        });
    };
}
