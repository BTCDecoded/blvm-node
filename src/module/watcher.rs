//! Module directory file watcher for hot-reload on change.
//!
//! When the `module-watcher` feature is enabled, watches the modules directory
//! for changes to module.toml, config.toml, or module binaries and triggers
//! reload for loaded modules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::module::manager::ModuleManager;
use crate::module::registry::discovery::ModuleDiscovery;
use crate::module::traits::ModuleError;

/// Debounce delay to avoid reload storms when multiple files change
const DEBOUNCE_MS: u64 = 500;

/// Watcher configuration (watch_auto_load, watch_auto_unload from config)
#[cfg(feature = "module-watcher")]
#[derive(Clone, Default)]
pub struct WatcherConfig {
    pub auto_load: bool,
    pub auto_unload: bool,
}

/// Module watcher: watches modules_dir and triggers reload on change
#[cfg(feature = "module-watcher")]
pub struct ModuleWatcher {
    modules_dir: PathBuf,
    module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    last_reload: Arc<Mutex<std::time::Instant>>,
    config: WatcherConfig,
}

#[cfg(feature = "module-watcher")]
impl ModuleWatcher {
    /// Create a new module watcher
    pub fn new(
        modules_dir: impl AsRef<Path>,
        module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    ) -> Self {
        Self::with_config(modules_dir, module_manager, WatcherConfig::default())
    }

    /// Create a new module watcher with config (auto_load, auto_unload)
    pub fn with_config(
        modules_dir: impl AsRef<Path>,
        module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
        config: WatcherConfig,
    ) -> Self {
        Self {
            modules_dir: modules_dir.as_ref().to_path_buf(),
            module_manager,
            last_reload: Arc::new(Mutex::new(std::time::Instant::now())),
            config,
        }
    }

    /// Start watching. Spawns a background task. Runs until the process exits.
    pub fn start(self: Arc<Self>) -> Result<tokio::task::JoinHandle<()>, ModuleError> {
        use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
        use std::sync::mpsc;

        let watcher_self = Arc::clone(&self);
        let modules_dir = self.modules_dir.clone();

        let (std_tx, std_rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = std_tx.send(event);
                }
            },
            Config::default(),
        )
        .map_err(|e| ModuleError::op_err("Watcher init failed", e))?;

        watcher
            .watch(&modules_dir, RecursiveMode::Recursive)
            .map_err(|e| ModuleError::op_err("Watch failed", e))?;

        info!("Module watcher started for {:?}", modules_dir);

        let (async_tx, mut async_rx) = tokio::sync::mpsc::channel::<notify::Event>(32);
        std::thread::spawn(move || {
            // Keep watcher alive - dropping it would stop watching
            let _watcher = watcher;
            while let Ok(ev) = std_rx.recv() {
                if async_tx.blocking_send(ev).is_err() {
                    break;
                }
            }
        });

        let handle = tokio::spawn(async move {
            let mut debounce = tokio::time::interval(Duration::from_millis(DEBOUNCE_MS));
            debounce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut pending_reload: Option<String> = None;
            let mut pending_unload: Option<String> = None;

            loop {
                tokio::select! {
                    event = async_rx.recv() => {
                        if let Some(ev) = event {
                            match ev.kind {
                                EventKind::Modify(_) | EventKind::Create(_) => {
                                    for path in &ev.paths {
                                        if let Some(name) = watcher_self.module_name_from_path(path) {
                                            pending_reload = Some(name);
                                            break;
                                        }
                                    }
                                }
                                EventKind::Remove(_) => {
                                    for path in &ev.paths {
                                        if let Some(name) = watcher_self.module_name_from_path(path) {
                                            pending_unload = Some(name);
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            break;
                        }
                    }
                    _ = debounce.tick() => {
                        if let Some(name) = pending_unload.take() {
                            if watcher_self.config.auto_unload {
                                if let Err(e) = watcher_self.try_unload(&name).await {
                                    warn!("Module watcher unload failed for {}: {}", name, e);
                                }
                            }
                        }
                        if let Some(name) = pending_reload.take() {
                            if let Err(e) = watcher_self.try_reload_or_load(&name).await {
                                warn!("Module watcher reload/load failed for {}: {}", name, e);
                            }
                        }
                    }
                }
            }
        });

        Ok(handle)
    }

    fn module_name_from_path(&self, path: &Path) -> Option<String> {
        let path = path.strip_prefix(&self.modules_dir).ok()?;
        let components: Vec<_> = path.components().collect();
        if components.is_empty() {
            return None;
        }
        let first = components[0].as_os_str().to_str()?;
        let rest = path.strip_prefix(Path::new(first)).ok()?;
        let rest_str = rest.to_string_lossy();
        // Module dir itself (Remove) or module.toml/config.toml/binary
        if rest.as_os_str().is_empty()
            || rest == Path::new("module.toml")
            || rest == Path::new("config.toml")
            || rest_str.contains("target/release/")
        {
            Some(first.to_string())
        } else {
            None
        }
    }

    async fn try_unload(&self, name: &str) -> Result<(), ModuleError> {
        let mut manager = self.module_manager.lock().await;
        if manager.list_modules().await.contains(&name.to_string()) {
            info!(
                "Module watcher: auto-unloading {} after directory removal",
                name
            );
            manager.unload_module(name).await
        } else {
            Ok(())
        }
    }

    async fn try_reload_or_load(&self, name: &str) -> Result<(), ModuleError> {
        let mut last = self.last_reload.lock().await;
        if last.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
            return Ok(());
        }
        *last = std::time::Instant::now();
        drop(last);

        let mut manager = self.module_manager.lock().await;
        let loaded = manager.list_modules().await.contains(&name.to_string());
        if loaded {
            let discovery = ModuleDiscovery::new(manager.modules_dir());
            let discovered = discovery.discover_module(name)?;
            let config = crate::module::loader::ModuleLoader::load_module_config(
                name,
                &discovered.directory.join("config.toml"),
            );
            info!("Module watcher: reloading {} after file change", name);
            manager
                .reload_module(
                    name,
                    &discovered.binary_path,
                    discovered.manifest.to_metadata(),
                    config,
                )
                .await
        } else if self.config.auto_load {
            info!("Module watcher: auto-loading {} after file change", name);
            manager.auto_load_modules().await
        } else {
            debug!(
                "Module {} not loaded, skipping watcher (watch_auto_load=false)",
                name
            );
            Ok(())
        }
    }
}

#[cfg(not(feature = "module-watcher"))]
pub struct ModuleWatcher;

#[cfg(not(feature = "module-watcher"))]
impl ModuleWatcher {
    pub fn new(
        _modules_dir: impl AsRef<Path>,
        _module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    ) -> Self {
        Self
    }

    pub fn start(self: Arc<Self>) -> Result<tokio::task::JoinHandle<()>, ModuleError> {
        Err(ModuleError::OperationError(
            "Module watcher requires 'module-watcher' feature".to_string(),
        ))
    }
}
