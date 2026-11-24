use malak_layers::Error;
use parking_lot::{Condvar, Mutex};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const IDLE_WORKERS: usize = 8;

enum Job {
    Waited(Arc<dyn Run>),
    Detached(Box<dyn FnOnce() + Send>),
}

trait Run: Send + Sync {
    fn run(&self, recycle: &mut dyn FnMut());
}

struct Task<F, T> {
    work: Mutex<Option<F>>,
    reply: Slot<(Instant, Result<T, Error>)>,
}

impl<F, T> Run for Task<F, T>
where
    F: FnOnce() -> Result<T, Error> + Send,
    T: Send,
{
    fn run(&self, recycle: &mut dyn FnMut()) {
        let work = self.work.lock().take().expect("task dispatched once");
        let result = catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err(Error::unavailable("worker stopped")));
        let completed = Instant::now();
        // Publish the idle slot before waking a caller that may immediately
        // make another request. A busy worker never enters the idle list.
        recycle();
        self.reply.put((completed, result));
    }
}

type Mailbox = Slot<Option<Job>>;

#[derive(Default)]
struct Idle {
    workers: Mutex<Vec<Arc<Mailbox>>>,
}

impl Drop for Idle {
    fn drop(&mut self) {
        for worker in self.workers.get_mut().drain(..) {
            worker.put(None);
        }
    }
}

/// Idle workers can be reused, but a job never queues behind a busy worker.
pub(crate) struct Workers {
    idle: Arc<Idle>,
    thread_name: &'static str,
}

impl Default for Workers {
    fn default() -> Self {
        Self::named("malak-store")
    }
}

impl Workers {
    pub(crate) fn named(thread_name: &'static str) -> Self {
        Self {
            idle: Arc::default(),
            thread_name,
        }
    }

    pub(crate) fn until<T: Send + 'static>(
        &self,
        deadline: Instant,
        work: impl FnOnce() -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        self.until_with(|| deadline, work)
    }

    pub(crate) fn until_with<T: Send + 'static>(
        &self,
        deadline: impl Fn() -> Instant,
        work: impl FnOnce() -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let task = Arc::new(Task {
            work: Mutex::new(Some(work)),
            reply: Slot::new(),
        });
        self.submit(Job::Waited(task.clone()))?;
        match task.reply.take(Some(&deadline)) {
            Some((completed, result)) if completed <= deadline() => result,
            _ => Err(Error::unavailable("worker did not answer in time")),
        }
    }

    pub(crate) fn dispatch(&self, work: impl FnOnce() + Send + 'static) -> Result<(), Error> {
        self.submit(Job::Detached(Box::new(work)))
    }

    fn submit(&self, mut job: Job) -> Result<(), Error> {
        let worker = self.idle.workers.lock().pop();
        if let Some(worker) = worker {
            worker.put(Some(job));
            return Ok(());
        }

        let idle = Arc::downgrade(&self.idle);
        thread::Builder::new()
            .name(self.thread_name.into())
            .spawn(move || {
                let mailbox = Arc::new(Mailbox::new());
                loop {
                    let mut recycled = false;
                    let mut recycle = || {
                        if let Some(idle) = idle.upgrade() {
                            let mut workers = idle.workers.lock();
                            if workers.len() < IDLE_WORKERS {
                                workers.push(mailbox.clone());
                                recycled = true;
                            }
                        }
                    };
                    match job {
                        Job::Waited(task) => task.run(&mut recycle),
                        Job::Detached(work) => {
                            work();
                            recycle();
                        }
                    }
                    if !recycled {
                        break;
                    }
                    let Some(next) = mailbox.take(None::<fn() -> Instant>).flatten() else {
                        break;
                    };
                    job = next;
                }
            })
            .map_err(|_| Error::unavailable("could not start a worker"))?;
        Ok(())
    }
}

/// A one-value mailbox with one consumer and a single producer at a time.
struct Slot<T> {
    value: Mutex<Option<T>>,
    changed: Condvar,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            value: Mutex::new(None),
            changed: Condvar::new(),
        }
    }

    fn put(&self, value: T) {
        let mut slot = self.value.lock();
        debug_assert!(slot.is_none());
        *slot = Some(value);
        self.changed.notify_one();
    }

    fn take(&self, deadline: Option<impl Fn() -> Instant>) -> Option<T> {
        let mut slot = self.value.lock();
        while slot.is_none() {
            if let Some(deadline) = &deadline {
                if self.changed.wait_until(&mut slot, deadline()).timed_out()
                    && slot.is_none()
                    && Instant::now() >= deadline()
                {
                    return None;
                }
            } else {
                self.changed.wait(&mut slot);
            }
        }
        slot.take()
    }
}
