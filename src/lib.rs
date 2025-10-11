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
use tauri_specta::Event;
use tracing::{debug, error};

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
pub struct Item<'r, T: ItemTrait>(&'r Mutex<T>, &'r str, &'r AppHandle);

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
    }
}

impl<'r, T: ItemTrait> Clone for Item<'r, T> {
    fn clone(&self) -> Self {
        Item(self.0, self.1, self.2)
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
    from_str: Box<dyn Fn(&str) -> Result<Box<dyn Any + Send>, serde_json::Error> + Send>,
    to_str: Box<dyn Fn(&dyn Any) -> Result<String, serde_json::Error> + Send>,
}

type MapAny = HashMap<String, Pin<Box<dyn Any + Send + Sync>>>;
type SerializersMap = HashMap<String, Serializers>;

#[derive(Clone)]
pub struct StateSyncer {
    data: Arc<Mutex<MapAny>>,
    serializers: Arc<Mutex<SerializersMap>>,
    app: AppHandle,
}

impl StateSyncer {
    pub fn new(app: AppHandle) -> Self {
        let syncer = StateSyncer {
            data: Default::default(),
            serializers: Default::default(),
            app: app.clone(),
        };

        syncer
    }

    pub fn update_typed_string<'a, T: ItemTrait>(&self, key: &str, value: &'a str) {
        debug!(key, "update_typed_string");
        let new_value: T = match serde_json::from_str(value) {
            Ok(res) => res,
            Err(_) => {
                error!("failed to parse internal state");
                return;
            }
        };

        self.update(key, new_value);
    }

    pub fn update<'a, T: ItemTrait>(&self, key: &str, new_value: T) {
        debug!(key, "update: {:?}", new_value);
        let mut guard = self.data.lock().unwrap();
        if !guard.contains_key(key) {
            debug!("key doesn't already exist, inserting instead");
            guard.insert(key.to_string(), Box::pin(Mutex::new(new_value)));
            return;
        }

        let ptr = guard.get(key).unwrap();
        let value = unsafe {
            ptr.downcast_ref::<Mutex<T>>()
                // SAFETY: the type of the key is the same as the type of the value
                .unwrap_unchecked()
        };
        let v_ref = unsafe { &*(value as *const Mutex<T>) };

        let mut v_guard = v_ref.lock().unwrap();
        *v_guard = new_value;
    }

    pub fn set<'a, T: ItemTrait>(&self, key: &str, value: T) {
        debug!(key, "set");

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
                    from_str: Box::new(deserializer),
                    to_str: Box::new(serializer),
                };

                ds_guard.insert(key.to_string(), s);
            }
        }

        let mut map_guard = self.data.lock().unwrap();
        map_guard.insert(key.to_string(), Box::pin(Mutex::new(value)));
    }

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

        Item(v_ref, key, &self.app)
    }

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
                            .update_typed_string::<$state_type>($state_name, state.value.as_str());
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
                        );
                    }
                )*
                _ => return,
            }
        });
    };
}
