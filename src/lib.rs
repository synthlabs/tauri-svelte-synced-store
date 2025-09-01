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

// Define a helper trait that combines all the required traits
pub trait ItemTrait<'r>:
    'static + Send + Sync + Serialize + Deserialize<'r> + Debug + Clone
{
}
// Blanket impl
impl<'r, T> ItemTrait<'r> for T where
    T: 'static + Send + Sync + Serialize + Deserialize<'r> + Debug + Clone
{
}

pub struct Item<'r, T: ItemTrait<'r>>(&'r Mutex<T>, &'r str, &'r AppHandle);

impl<'r, T: ItemTrait<'r>> Item<'r, T> {
    pub fn lock(&'_ self) -> LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }
}

impl<'r, T: ItemTrait<'r>> Drop for Item<'r, T> {
    fn drop(&mut self) {
        let self_guard = self.0.lock().unwrap();
        debug!("[Item] dropped: {:?}", *self_guard);

        let name = format!("{}_update", self.1);
        self.2
            .emit(&name, self_guard.clone())
            .expect("unable to emit state");
    }
}

impl<'r, T: ItemTrait<'r>> Clone for Item<'r, T> {
    fn clone(&self) -> Self {
        Item(self.0, self.1, self.2)
    }
}

impl<'r, T: ItemTrait<'r> + PartialEq> PartialEq for Item<'r, T> {
    fn eq(&self, other: &Self) -> bool {
        let self_guard = self.0.lock().unwrap();
        let other_guard = other.0.lock().unwrap();
        self_guard.eq(&other_guard) && self.1 == other.1
    }
}

type MapAny = HashMap<String, Pin<Box<dyn Any + Send + Sync>>>;

#[derive(Clone)]
pub struct StateSyncer {
    data: Arc<Mutex<MapAny>>,
    app: AppHandle,
}

impl StateSyncer {
    pub fn new(app: AppHandle) -> Self {
        let syncer = StateSyncer {
            data: Default::default(),
            app: app.clone(),
        };

        syncer
    }

    pub fn update_string<'a, T>(&self, key: &str, value: &'a str)
    where
        T: 'static + Send + Sync + Serialize + Deserialize<'a> + Debug,
    {
        debug!(key, "update_string");
        let new_value: T = match serde_json::from_str(value) {
            Ok(res) => res,
            Err(_) => {
                error!("failed to parse internal state");
                return;
            }
        };

        self.update(key, new_value);
    }

    pub fn update<'a, T>(&self, key: &str, new_value: T)
    where
        T: 'static + Send + Sync + Serialize + Deserialize<'a> + Debug,
    {
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

    pub fn set<'a, T>(&self, key: &str, value: T)
    where
        T: 'static + Send + Sync + Serialize + Deserialize<'a>,
    {
        debug!(key, "set");
        let mut guard = self.data.lock().unwrap();
        guard.insert(key.to_string(), Box::pin(Mutex::new(value)));
    }

    pub fn get<'a, T>(&'a self, key: &'a str) -> Item<'a, T>
    where
        T: 'static + Send + Sync + Serialize + Deserialize<'a> + Debug + Clone,
    {
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

    pub fn emit<'a, T>(&self, name: &str) -> bool
    where
        T: 'static + Send + Sync + Serialize + Deserialize<'a> + Clone + Debug + Clone,
    {
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
