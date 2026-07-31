import logging
import threading
from dataclasses import dataclass, field
from enum import Enum
from typing import Optional

logger = logging.getLogger(__name__)


class TaskStatus(Enum):
    PENDING = "pending"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"


class TaskType(Enum):
    SYNC_USER = "sync_user"
    BACKFILL = "backfill"
    EXPORT = "export"


@dataclass
class TaskError:
    error_type: str
    message: str
    item_id: Optional[str] = None


@dataclass
class Task:
    id: int
    task_type: TaskType
    description: str
    status: TaskStatus = TaskStatus.PENDING
    progress: int = 0
    total: int = 0
    error: Optional[str] = None


class TaskEventListener:
    def on_task_updated(self, task: Task):
        pass

    def on_task_error(self, error: TaskError):
        pass


class TaskManager:
    _instance = None
    _lock = threading.Lock()

    def __new__(cls):
        if cls._instance is None:
            with cls._lock:
                if cls._instance is None:
                    cls._instance = super().__new__(cls)
                    cls._instance._initialized = False
        return cls._instance

    def __init__(self):
        if self._initialized:
            return
        self._initialized = True
        self._task: Optional[Task] = None
        self._errors: list[TaskError] = []
        self._listeners: list[TaskEventListener] = []
        self._next_id = 1
        self._task_lock = threading.Lock()
        self._errors_lock = threading.Lock()
        self._listeners_lock = threading.Lock()

    def start_task(self, task_type: TaskType, description: str, total: int = 0) -> Task:
        with self._task_lock:
            if self._task and self._task.status == TaskStatus.RUNNING:
                raise RuntimeError("Another task is already running")
            task = Task(
                id=self._next_id,
                task_type=task_type,
                description=description,
                status=TaskStatus.RUNNING,
                total=total,
            )
            self._next_id += 1
            self._task = task
        self._notify_update()
        return task

    def update_progress(self, progress: int, total: Optional[int] = None):
        with self._task_lock:
            if not self._task or self._task.status != TaskStatus.RUNNING:
                return
            self._task.progress = progress
            if total is not None:
                self._task.total = total
            task_copy = Task(**self._task.__dict__)
        self._notify_update(task_copy)

    def finish_task(self):
        with self._task_lock:
            if not self._task:
                return
            self._task.status = TaskStatus.COMPLETED
            self._task.progress = self._task.total
            task_copy = Task(**self._task.__dict__)
        self._notify_update(task_copy)

    def fail_task(self, error: str):
        with self._task_lock:
            if not self._task:
                return
            self._task.status = TaskStatus.FAILED
            self._task.error = error
            task_copy = Task(**self._task.__dict__)
        self._notify_update(task_copy)

    def report_error(self, error_type: str, message: str, item_id: Optional[str] = None):
        err = TaskError(error_type=error_type, message=message, item_id=item_id)
        with self._errors_lock:
            self._errors.append(err)
        self._notify_error(err)

    def get_and_clear_errors(self) -> list[TaskError]:
        with self._errors_lock:
            errors = list(self._errors)
            self._errors.clear()
        return errors

    def get_errors(self) -> list[TaskError]:
        with self._errors_lock:
            return list(self._errors)

    def get_current_task(self) -> Optional[Task]:
        with self._task_lock:
            if self._task:
                return Task(**self._task.__dict__)
            return None

    def add_listener(self, listener: TaskEventListener):
        with self._listeners_lock:
            self._listeners.append(listener)

    def remove_listener(self, listener: TaskEventListener):
        with self._listeners_lock:
            self._listeners.remove(listener)

    def _notify_update(self, task: Optional[Task] = None):
        with self._listeners_lock:
            listeners = list(self._listeners)
        if task is None:
            with self._task_lock:
                if self._task:
                    task = Task(**self._task.__dict__)
        if task:
            for l in listeners:
                try:
                    l.on_task_updated(task)
                except Exception:
                    logger.exception("Listener on_task_updated failed")

    def _notify_error(self, error: TaskError):
        with self._listeners_lock:
            listeners = list(self._listeners)
        for l in listeners:
            try:
                l.on_task_error(error)
            except Exception:
                logger.exception("Listener on_task_error failed")
