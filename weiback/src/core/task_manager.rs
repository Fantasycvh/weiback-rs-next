//! This module provides the infrastructure for tracking the lifecycle of asynchronous tasks.
//!
//! The [`TaskManager`] allows the application to:
//! - Monitor the progress of a currently running task.
//! - Retrieve error messages if a task or its tasks fail.
//! - Ensure that only one long-running task is active at a time.

use serde::Serialize;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// The general category of an asynchronous task.
#[derive(Debug, Clone, Serialize)]
pub enum TaskType {
    /// Backup posts from a specific user.
    BackupUser,
    /// Backup favorited posts.
    BackupFavorites,
    /// Unfavorite posts that are already in local storage but still favorited on Weibo.
    UnfavoritePosts,
    /// Export posts from local storage to external formats.
    Export,
    /// Clean up redundant or low-resolution images.
    CleanupPictures,
    /// Clean up invalid or outdated avatars.
    CleanupAvatars,
    /// Clean up invalid posts (e.g., user is None).
    CleanupInvalidPosts,
    /// Re-backup posts based on a query.
    RebackupPosts,
    /// Re-backup posts that have missing images.
    RebackupMissingImages,
    /// Clean up invalid pictures (e.g., "image deleted" placeholders).
    CleanupInvalidPictures,
    /// Collect a user's posts through the Python sidecar.
    CollectUserPosts,
    /// Collect first-level comments through the Python sidecar.
    CollectComments,
    /// Collect replies to a comment through the Python sidecar.
    CollectCommentReplies,
}

/// The current execution state of a task.
///
/// 状态机（见 PLAN §6.4）：
/// `pending -> running(InProgress) -> completed`
///                              `-> failed`
///                              `-> paused -> running`
///                              `-> cancelled`
/// `running --进程退出--> interrupted -> running`
///
/// 序列化保持既有 PascalCase 变体名，兼容现有前端（`Completed`/`Failed`/`InProgress`）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum TaskStatus {
    /// The task has been created but not yet started.
    Pending,
    /// The task is currently running.
    InProgress,
    /// The task has finished successfully.
    Completed,
    /// The task has stopped due to a fatal error.
    Failed,
    /// The task was temporarily paused and can be resumed.
    Paused,
    /// The task was cancelled by the user.
    Cancelled,
    /// The task was interrupted by process exit and can be resumed.
    Interrupted,
}

/// Represents a single unit of work being performed by the application.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    /// The unique task ID.
    pub id: u64,
    /// The general category of the task.
    pub task_type: TaskType,
    /// A human-readable summary of the task.
    pub description: String,
    /// The current state of the task (InProgress, Completed, Failed).
    pub status: TaskStatus,
    /// Current completion progress (e.g., number of pages fetched).
    pub progress: u64,
    /// The total estimated progress for completion.
    pub total: u64,
    /// An optional error message if the task failed.
    pub error: Option<String>,
}

/// Types of errors that can occur within a task (e.g., individual file download).
#[derive(Debug, Clone, Serialize)]
pub enum TaskErrorType {
    /// Failed to download a specific media file. Contains the URL.
    DownloadMedia(String),
}

/// A non-fatal error record for a specific operation within a larger task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskError {
    /// The category of the error.
    pub error_type: TaskErrorType,
    /// A detailed error message.
    pub message: String,
}

/// A trait for listening to task-related events.
///
/// Implementations of this trait can receive real-time updates when a task's
/// progress changes or when task errors occur.
pub trait TaskEventListener: Send + Sync {
    /// Called when a task's state or progress is updated.
    fn on_task_updated(&self, task: &Task);
    /// Called when a non-fatal task error is recorded.
    fn on_task_error(&self, error: &TaskError);
}

/// A thread-safe manager for monitoring the execution state of application tasks.
///
/// `TaskManager` ensures that long-running operations can be monitored from the
/// UI and prevents multiple conflicting tasks from running simultaneously.
#[derive(Clone, Default)]
pub struct TaskManager {
    current_task: Arc<Mutex<Option<Task>>>,
    task_errors: Arc<Mutex<Vec<TaskError>>>,
    listener: Arc<Mutex<Option<Box<dyn TaskEventListener>>>>,
}

impl std::fmt::Debug for TaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskManager")
            .field("current_task", &self.current_task)
            .field("task_errors", &self.task_errors)
            .field("listener", &"Option<Box<dyn TaskEventListener>>")
            .finish()
    }
}

impl TaskManager {
    /// Creates a new, empty `TaskManager`.
    pub fn new() -> Self {
        Self {
            current_task: Arc::new(Mutex::new(None)),
            task_errors: Arc::new(Mutex::new(Vec::new())),
            listener: Arc::new(Mutex::new(None)),
        }
    }

    /// Sets the task event listener.
    pub fn set_listener(&self, listener: Box<dyn TaskEventListener>) -> Result<()> {
        let mut listener_guard = self.listener.lock()?;
        *listener_guard = Some(listener);
        Ok(())
    }

    /// Registers and starts a new task.
    ///
    /// # Arguments
    /// * `id` - A unique identifier for the task.
    /// * `task_type` - The category of the task.
    /// * `description` - A human-readable description of what the task does.
    /// * `total` - The initial estimate of total work units (can be updated later).
    ///
    /// # Errors
    /// Returns `Error::InconsistentTask` if another task is already `InProgress`.
    pub fn start_task(
        &self,
        id: u64,
        task_type: TaskType,
        description: String,
        total: u64,
    ) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        if let Some(existing_task) = task_guard.as_ref()
            && existing_task.status == TaskStatus::InProgress
        {
            return Err(Error::InconsistentTask(
                "Another task is already in progress.".to_string(),
            ));
        }

        let new_task = Task {
            id,
            task_type,
            description,
            status: TaskStatus::InProgress,
            progress: 0,
            total,
            error: None,
        };
        *task_guard = Some(new_task.clone());

        if let Some(listener) = self.listener.lock()?.as_ref() {
            listener.on_task_updated(&new_task);
        }
        Ok(())
    }

    /// Updates the progress and total units of the currently active task.
    ///
    /// # Arguments
    /// * `progress` - The new progress value (absolute, not incremental).
    /// * `total` - The new total units value (absolute, not incremental).
    ///
    /// # Errors
    /// Returns `Error::InconsistentTask` if no task is currently `InProgress`.
    pub fn update_progress(&self, progress: u64, total: u64) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        if let Some(task) = task_guard.as_mut() {
            if task.status == TaskStatus::InProgress {
                task.progress = progress;
                task.total = total;
                let task_clone = task.clone();
                if let Some(listener) = self.listener.lock()?.as_ref() {
                    listener.on_task_updated(&task_clone);
                }
            }
            Ok(())
        } else {
            Err(Error::InconsistentTask(
                "Cannot update progress: no task is in progress.".to_string(),
            ))
        }
    }

    /// Updates progress only when `task_id` still identifies the active task.
    pub fn update_progress_for(&self, task_id: u64, progress: u64, total: u64) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        let task = Self::active_task_for(&mut task_guard, task_id, "update progress")?;
        task.progress = progress;
        task.total = total;
        let task_clone = task.clone();
        drop(task_guard);
        self.notify(&task_clone)
    }

    /// Marks the current task as `Completed`.
    ///
    /// # Errors
    /// Returns `Error::InconsistentTask` if no task is currently active.
    pub fn finish(&self) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        if let Some(task) = task_guard.as_mut() {
            task.status = TaskStatus::Completed;
            let task_clone = task.clone();
            if let Some(listener) = self.listener.lock()?.as_ref() {
                listener.on_task_updated(&task_clone);
            }
            Ok(())
        } else {
            Err(Error::InconsistentTask(
                "Cannot finish task: no task is in progress.".to_string(),
            ))
        }
    }

    /// Marks the matching active task as completed.
    pub fn finish_for(&self, task_id: u64) -> Result<()> {
        self.transition_for(task_id, TaskStatus::Completed, None, "finish")
    }

    /// Marks the current task as `Failed` and records an error message.
    ///
    /// # Arguments
    /// * `error` - The error message explaining the failure.
    ///
    /// # Errors
    /// Returns `Error::InconsistentTask` if no task is currently active.
    pub fn fail(&self, error: String) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        if let Some(task) = task_guard.as_mut() {
            task.status = TaskStatus::Failed;
            task.error = Some(error);
            let task_clone = task.clone();
            if let Some(listener) = self.listener.lock()?.as_ref() {
                listener.on_task_updated(&task_clone);
            }
            Ok(())
        } else {
            Err(Error::InconsistentTask(
                "Cannot fail task: no task is in progress.".to_string(),
            ))
        }
    }

    /// Marks the matching active task as failed.
    pub fn fail_for(&self, task_id: u64, error: String) -> Result<()> {
        self.transition_for(task_id, TaskStatus::Failed, Some(error), "fail")
    }

    /// Reports a non-fatal task error.
    ///
    /// These errors do not stop the main task but are reported.
    ///
    /// # Arguments
    /// * `error` - The `TaskError` to report.
    pub fn report_task_error(&self, error: TaskError) -> Result<()> {
        self.task_errors.lock()?.push(error.clone());
        if let Some(listener) = self.listener.lock()?.as_ref() {
            listener.on_task_error(&error);
        }
        Ok(())
    }

    /// Retrieves all recorded task errors and clears the internal list.
    ///
    /// # Returns
    /// A `Result` containing a `Vec` of `TaskError`s.
    pub fn get_and_clear_task_errors(&self) -> Result<Vec<TaskError>> {
        let mut errors = self.task_errors.lock()?;
        let ret = errors.drain(..).collect();
        Ok(ret)
    }

    /// Returns a clone of the currently registered task, if any.
    pub fn get_current(&self) -> Result<Option<Task>> {
        Ok(self.current_task.lock()?.clone())
    }

    /// 暂停当前任务（仅 `InProgress` → `Paused`）。
    pub fn pause_current(&self) -> Result<()> {
        self.transition_if(
            TaskStatus::InProgress,
            TaskStatus::Paused,
            "Cannot pause task: it is not in progress.".to_string(),
        )
    }

    /// Pauses the matching active task.
    pub fn pause_for(&self, task_id: u64) -> Result<()> {
        self.transition_for(task_id, TaskStatus::Paused, None, "pause")
    }

    /// 取消当前任务（`InProgress`/`Paused` → `Cancelled`）。
    pub fn cancel_current(&self) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        let Some(task) = task_guard.as_mut() else {
            return Err(Error::InconsistentTask(
                "Cannot cancel task: no active task.".to_string(),
            ));
        };
        if task.status != TaskStatus::InProgress && task.status != TaskStatus::Paused {
            return Err(Error::InconsistentTask(format!(
                "Cannot cancel task: current status is {:?}.",
                task.status
            )));
        }
        task.status = TaskStatus::Cancelled;
        let task_clone = task.clone();
        drop(task_guard);
        self.notify(&task_clone)?;
        Ok(())
    }

    /// Cancels the matching active task.
    pub fn cancel_for(&self, task_id: u64) -> Result<()> {
        self.transition_for(task_id, TaskStatus::Cancelled, None, "cancel")
    }

    /// 标记当前任务为进程中断（仅 `InProgress` → `Interrupted`），
    /// 用于应用启动时把遗留的 running 任务转为可恢复状态。
    pub fn interrupt_current(&self) -> Result<()> {
        self.transition_if(
            TaskStatus::InProgress,
            TaskStatus::Interrupted,
            "Cannot interrupt task: it is not in progress.".to_string(),
        )
    }

    /// Marks the matching active task as interrupted.
    pub fn interrupt_for(&self, task_id: u64) -> Result<()> {
        self.transition_for(task_id, TaskStatus::Interrupted, None, "interrupt")
    }

    /// 恢复被暂停或中断的任务（`Paused`/`Interrupted` → `InProgress`）。
    pub fn resume_current(&self) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        let Some(task) = task_guard.as_mut() else {
            return Err(Error::InconsistentTask(
                "Cannot resume task: no active task.".to_string(),
            ));
        };
        if task.status != TaskStatus::Paused && task.status != TaskStatus::Interrupted {
            return Err(Error::InconsistentTask(format!(
                "Cannot resume task: current status is {:?}.",
                task.status
            )));
        }
        task.status = TaskStatus::InProgress;
        let task_clone = task.clone();
        drop(task_guard);
        self.notify(&task_clone)?;
        Ok(())
    }

    /// 从 `from` 状态迁移到 `to` 状态并通知监听器。
    fn transition_if(&self, from: TaskStatus, to: TaskStatus, err_msg: String) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        let Some(task) = task_guard.as_mut() else {
            return Err(Error::InconsistentTask(
                "Cannot transition task: no active task.".to_string(),
            ));
        };
        if task.status != from {
            return Err(Error::InconsistentTask(err_msg));
        }
        task.status = to;
        let task_clone = task.clone();
        drop(task_guard);
        self.notify(&task_clone)?;
        Ok(())
    }

    fn active_task_for<'a>(
        task_guard: &'a mut Option<Task>,
        task_id: u64,
        operation: &str,
    ) -> Result<&'a mut Task> {
        let Some(task) = task_guard.as_mut() else {
            return Err(Error::InconsistentTask(format!(
                "Cannot {operation}: no active task."
            )));
        };
        if task.id != task_id || task.status != TaskStatus::InProgress {
            return Err(Error::InconsistentTask(format!(
                "Cannot {operation}: task {task_id} is not the active in-progress task."
            )));
        }
        Ok(task)
    }

    fn transition_for(
        &self,
        task_id: u64,
        status: TaskStatus,
        error: Option<String>,
        operation: &str,
    ) -> Result<()> {
        let mut task_guard = self.current_task.lock()?;
        let task = Self::active_task_for(&mut task_guard, task_id, operation)?;
        task.status = status;
        task.error = error;
        let task_clone = task.clone();
        drop(task_guard);
        self.notify(&task_clone)
    }

    /// 通知监听器任务已更新。
    fn notify(&self, task: &Task) -> Result<()> {
        if let Some(listener) = self.listener.lock()?.as_ref() {
            listener.on_task_updated(task);
        }
        Ok(())
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;

    #[test]
    fn test_start_new_task() {
        let manager = TaskManager::new();
        assert!(manager.get_current().unwrap().is_none());

        manager
            .start_task(1, TaskType::BackupUser, "Test task".into(), 10)
            .unwrap();

        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.id, 1);
        assert_eq!(task.description, "Test task");
        assert_eq!(task.status, TaskStatus::InProgress);
        assert_eq!(task.progress, 0);
        assert_eq!(task.total, 10);
        assert!(task.error.is_none());
    }

    #[test]
    fn test_prevent_starting_task_when_in_progress() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "First task".into(), 10)
            .unwrap();
        let result = manager.start_task(2, TaskType::BackupFavorites, "Second task".into(), 5);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::InconsistentTask(msg) => {
                assert!(msg.contains("Another task is already in progress."));
            }
            _ => panic!("Expected InconsistentTask error"),
        }
    }

    #[test]
    fn test_update_progress() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "Test task".into(), 10)
            .unwrap();

        manager.update_progress(5, 10).unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.progress, 5);
        assert_eq!(task.total, 10);

        manager.update_progress(6, 10).unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.progress, 6);
        assert_eq!(task.total, 10);
    }

    #[test]
    fn test_finish_task() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "Test task".into(), 10)
            .unwrap();
        manager.finish().unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
    }

    #[test]
    fn test_fail_task() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "Test task".into(), 10)
            .unwrap();
        let error_msg = "Something went wrong".to_string();
        manager.fail(error_msg.clone()).unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error, Some(error_msg));
    }

    #[test]
    fn test_task_error_handling() {
        let manager = TaskManager::new();
        assert!(manager.get_and_clear_task_errors().unwrap().is_empty());

        let error1 = TaskError {
            error_type: TaskErrorType::DownloadMedia("url1".into()),
            message: "404 Not Found".into(),
        };
        let error2 = TaskError {
            error_type: TaskErrorType::DownloadMedia("url2".into()),
            message: "Timeout".into(),
        };

        manager.report_task_error(error1.clone()).unwrap();
        manager.report_task_error(error2.clone()).unwrap();

        let errors = manager.get_and_clear_task_errors().unwrap();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].message, "404 Not Found");
        assert_eq!(errors[1].message, "Timeout");

        // Verify that the error list is cleared
        assert!(manager.get_and_clear_task_errors().unwrap().is_empty());
    }

    #[test]
    fn test_start_new_task_after_completion() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "First task".into(), 10)
            .unwrap();
        manager.finish().unwrap();

        // Should be able to start a new task
        let result = manager.start_task(2, TaskType::BackupFavorites, "Second task".into(), 5);
        assert!(result.is_ok());
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.id, 2);
        assert_eq!(task.status, TaskStatus::InProgress);
    }

    #[test]
    fn test_state_machine_pause_cancel_interrupt_resume() {
        let manager = TaskManager::new();

        // 中断非法状态：无任务。
        assert!(manager.interrupt_current().is_err());

        manager
            .start_task(1, TaskType::BackupUser, "Test".into(), 10)
            .unwrap();

        // running -> paused -> running
        manager.pause_current().unwrap();
        assert_eq!(
            manager.get_current().unwrap().unwrap().status,
            TaskStatus::Paused
        );
        manager.resume_current().unwrap();
        assert_eq!(
            manager.get_current().unwrap().unwrap().status,
            TaskStatus::InProgress
        );

        // running -> interrupted -> running
        manager.interrupt_current().unwrap();
        assert_eq!(
            manager.get_current().unwrap().unwrap().status,
            TaskStatus::Interrupted
        );
        manager.resume_current().unwrap();
        assert_eq!(
            manager.get_current().unwrap().unwrap().status,
            TaskStatus::InProgress
        );

        // running -> cancelled
        manager.cancel_current().unwrap();
        assert_eq!(
            manager.get_current().unwrap().unwrap().status,
            TaskStatus::Cancelled
        );

        // 已取消不能再 pause/interrupt/resume。
        assert!(manager.pause_current().is_err());
        assert!(manager.interrupt_current().is_err());
        assert!(manager.resume_current().is_err());
    }

    #[test]
    fn test_start_task_blocked_while_running_but_allowed_after_cancel() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::BackupUser, "First".into(), 10)
            .unwrap();
        assert!(
            manager
                .start_task(2, TaskType::BackupFavorites, "Second".into(), 5)
                .is_err()
        );

        manager.cancel_current().unwrap();
        // cancelled 后允许新任务。
        assert!(
            manager
                .start_task(3, TaskType::BackupFavorites, "Third".into(), 5)
                .is_ok()
        );
        assert_eq!(manager.get_current().unwrap().unwrap().id, 3);
    }

    #[test]
    fn stale_task_id_cannot_mutate_replacement_task() {
        let manager = TaskManager::new();
        manager
            .start_task(1, TaskType::CollectUserPosts, "First".into(), 10)
            .unwrap();
        manager.cancel_current().unwrap();
        manager
            .start_task(2, TaskType::CollectComments, "Second".into(), 20)
            .unwrap();

        assert!(manager.update_progress_for(1, 7, 10).is_err());
        assert!(manager.finish_for(1).is_err());
        assert!(manager.fail_for(1, "stale failure".into()).is_err());

        let current = manager.get_current().unwrap().unwrap();
        assert_eq!(current.id, 2);
        assert_eq!(current.status, TaskStatus::InProgress);
        assert_eq!(current.progress, 0);
        assert!(current.error.is_none());
    }
}
