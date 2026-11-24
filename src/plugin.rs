use tauri::{
    Manager, RunEvent, Runtime, WindowEvent,
    plugin::{Builder, TauriPlugin},
    webview::PageLoadEvent,
};
use tracing::debug;

use crate::{StateSyncer, StateSyncerConfig};

/// Standard Tauri plugin that manages `StateSyncer<R>` and clears subscribers automatically on
/// webview page reloads (`PageLoadEvent::Started`) and window teardown (`WindowEvent::Destroyed`).
pub fn plugin<R: Runtime>(cfg: StateSyncerConfig) -> TauriPlugin<R> {
    Builder::new("svelte-synced-store")
        .setup(move |app, _api| {
            app.manage(StateSyncer::new(cfg, app.clone()));
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started
                && let Some(syncer) = webview.try_state::<StateSyncer<R>>()
            {
                let removed = syncer.clear_webview(webview.label());
                if removed > 0 {
                    debug!(
                        webview = webview.label(),
                        removed, "cleared synced state subscribers on page load start"
                    );
                }
            }
        })
        .on_event(|app, event| {
            if let RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } = event
                && let Some(syncer) = app.try_state::<StateSyncer<R>>()
            {
                let removed = syncer.clear_window(label);
                if removed > 0 {
                    debug!(
                        window = label,
                        removed, "cleared synced state subscribers on window destroy"
                    );
                }
            }
        })
        .build()
}
