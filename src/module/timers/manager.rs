//! Timer manager for module timers and scheduled tasks

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, warn};

/// Timer ID type
pub type TimerId = u64;
/// Task ID type
pub type TaskId = u64;

/// Async timer callback trait
#[async_trait]
pub trait TimerCallback: Send + Sync {
    /// Called when timer fires
    async fn call(&self) -> Result<(), String>;
}

/// Async task callback trait
#[async_trait]
pub trait TaskCallback: Send + Sync {
    /// Called when task executes
    async fn call(&self) -> Result<(), String>;
}

/// Timer entry
struct TimerEntry {
    id: TimerId,
    interval: Duration,
    callback: Arc<dyn TimerCallback>,
    last_fire: Instant,
    module_id: String,
}

/// Task entry
struct TaskEntry {
    id: TaskId,
    execute_at: Instant,
    callback: Arc<dyn TaskCallback>,
    module_id: String,
}

/// Timer manager for modules
pub struct TimerManager {
    /// Active timers
    timers: Arc<tokio::sync::RwLock<HashMap<TimerId, TimerEntry>>>,
    /// Scheduled tasks
    tasks: Arc<tokio::sync::RwLock<HashMap<TaskId, TaskEntry>>>,
    /// Next timer ID
    next_timer_id: Arc<tokio::sync::Mutex<TimerId>>,
    /// Next task ID
    next_task_id: Arc<tokio::sync::Mutex<TaskId>>,
    /// Handle to timer loop task
    timer_loop_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TimerManager {
    /// Create a new timer manager
    pub fn new() -> Self {
        Self {
            timers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            tasks: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            next_timer_id: Arc::new(tokio::sync::Mutex::new(1)),
            next_task_id: Arc::new(tokio::sync::Mutex::new(1)),
            timer_loop_handle: None,
        }
    }
    
    /// Start the timer loop
    pub fn start(&mut self) {
        if self.timer_loop_handle.is_some() {
            warn!("Timer loop already started");
            return;
        }
        
        let timers = Arc::clone(&self.timers);
        let tasks = Arc::clone(&self.tasks);
        
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                interval.tick().await;
                
                // Check timers
                let now = Instant::now();
                let timers_to_fire = {
                    let timers_guard = timers.read().await;
                    timers_guard
                        .values()
                        .filter(|timer| {
                            now.duration_since(timer.last_fire) >= timer.interval
                        })
                        .map(|timer| (timer.id, Arc::clone(&timer.callback), timer.module_id.clone()))
                        .collect::<Vec<_>>()
                };
                
                // Fire timers
                for (timer_id, callback, module_id) in timers_to_fire {
                    let callback_clone = Arc::clone(&callback);
                    tokio::spawn(async move {
                        match callback_clone.call().await {
                            Ok(()) => {
                                debug!("Timer {} fired successfully for module {}", timer_id, module_id);
                            }
                            Err(e) => {
                                error!("Timer {} callback error for module {}: {}", timer_id, module_id, e);
                            }
                        }
                    });
                    
                    // Update last_fire time
                    let mut timers_guard = timers.write().await;
                    if let Some(timer) = timers_guard.get_mut(&timer_id) {
                        timer.last_fire = now;
                    }
                }
                
                // Check scheduled tasks
                let tasks_to_execute = {
                    let tasks_guard = tasks.read().await;
                    let now = Instant::now();
                    tasks_guard
                        .iter()
                        .filter(|(_, task)| now >= task.execute_at)
                        .map(|(task_id, task)| (*task_id, Arc::clone(&task.callback), task.module_id.clone()))
                        .collect::<Vec<_>>()
                };
                
                // Execute tasks
                for (task_id, callback, module_id) in tasks_to_execute {
                    let callback_clone = Arc::clone(&callback);
                    tokio::spawn(async move {
                        match callback_clone.call().await {
                            Ok(()) => {
                                debug!("Task {} executed successfully for module {}", task_id, module_id);
                            }
                            Err(e) => {
                                error!("Task {} callback error for module {}: {}", task_id, module_id, e);
                            }
                        }
                    });
                    
                    // Remove task after execution
                    let mut tasks_guard = tasks.write().await;
                    tasks_guard.remove(&task_id);
                }
            }
        });
        
        self.timer_loop_handle = Some(handle);
        debug!("Timer loop started");
    }
    
    /// Stop the timer loop
    pub fn stop(&mut self) {
        if let Some(handle) = self.timer_loop_handle.take() {
            handle.abort();
            debug!("Timer loop stopped");
        }
    }
    
    /// Register a periodic timer
    pub async fn register_timer(
        &self,
        module_id: String,
        interval_seconds: u64,
        callback: Arc<dyn TimerCallback>,
    ) -> Result<TimerId, String> {
        let timer_id = {
            let mut id = self.next_timer_id.lock().await;
            let current = *id;
            *id = current.wrapping_add(1);
            current
        };
        
        let timer = TimerEntry {
            id: timer_id,
            interval: Duration::from_secs(interval_seconds),
            callback,
            last_fire: Instant::now(),
            module_id: module_id.clone(),
        };
        
        let mut timers = self.timers.write().await;
        timers.insert(timer_id, timer);
        
        debug!("Registered timer {} for module {} (interval: {}s)", timer_id, module_id, interval_seconds);
        Ok(timer_id)
    }
    
    /// Cancel a registered timer
    pub async fn cancel_timer(&self, timer_id: TimerId) -> Result<(), String> {
        let mut timers = self.timers.write().await;
        if timers.remove(&timer_id).is_some() {
            debug!("Cancelled timer {}", timer_id);
            Ok(())
        } else {
            Err(format!("Timer {} not found", timer_id))
        }
    }
    
    /// Cancel all timers for a module
    pub async fn cancel_module_timers(&self, module_id: &str) {
        let mut timers = self.timers.write().await;
        timers.retain(|_, timer| timer.module_id != module_id);
        debug!("Cancelled all timers for module {}", module_id);
    }
    
    /// Schedule a one-time task
    pub async fn schedule_task(
        &self,
        module_id: String,
        delay_seconds: u64,
        callback: Arc<dyn TaskCallback>,
    ) -> Result<TaskId, String> {
        let task_id = {
            let mut id = self.next_task_id.lock().await;
            let current = *id;
            *id = current.wrapping_add(1);
            current
        };
        
        let task = TaskEntry {
            id: task_id,
            execute_at: Instant::now() + Duration::from_secs(delay_seconds),
            callback,
            module_id: module_id.clone(),
        };
        
        let mut tasks = self.tasks.write().await;
        tasks.insert(task_id, task);
        
        debug!("Scheduled task {} for module {} (delay: {}s)", task_id, module_id, delay_seconds);
        Ok(task_id)
    }
    
    /// Cancel a scheduled task
    pub async fn cancel_task(&self, task_id: TaskId) -> Result<(), String> {
        let mut tasks = self.tasks.write().await;
        if tasks.remove(&task_id).is_some() {
            debug!("Cancelled task {}", task_id);
            Ok(())
        } else {
            Err(format!("Task {} not found", task_id))
        }
    }
    
    /// Cancel all tasks for a module
    pub async fn cancel_module_tasks(&self, module_id: &str) {
        let mut tasks = self.tasks.write().await;
        tasks.retain(|_, task| task.module_id != module_id);
        debug!("Cancelled all tasks for module {}", module_id);
    }
}

impl Default for TimerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TimerManager {
    fn drop(&mut self) {
        self.stop();
    }
}

