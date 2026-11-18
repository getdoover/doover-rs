//! The end-of-loop tag commit, beside the loop instead of in it.
//!
//! pydoover commits tags at the end of every loop and waits for the device
//! agent before the next loop starts. A commit takes tens of milliseconds on
//! a CM4, so a loop that wants 50 Hz runs at about 25. Here the loop only asks
//! for a commit and carries on; one task per app does the commits, one at a
//! time. Changes made while a commit is in flight stay buffered in the tags
//! runtime and go out together in the next commit, so the agent sees every
//! tag's latest value, at the rate it can take.
//!
//! A failed commit is reported at the end of the next loop, through the same
//! path as a failed in-loop commit: the app goes unhealthy and restarts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::Result;
use crate::tags::TagsRuntime;

pub(crate) struct TagCommitter {
    tags: Arc<TagsRuntime>,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

struct Shared {
    wake: Notify,
    stop: AtomicBool,
    failed: Mutex<Option<String>>,
}

impl TagCommitter {
    pub(crate) fn spawn(tags: Arc<TagsRuntime>) -> Self {
        let shared = Arc::new(Shared {
            wake: Notify::new(),
            stop: AtomicBool::new(false),
            failed: Mutex::new(None),
        });
        let task = tokio::spawn({
            let (tags, shared) = (tags.clone(), shared.clone());
            async move {
                loop {
                    // A request made during a commit leaves a permit, so the
                    // next commit starts as soon as this one ends.
                    shared.wake.notified().await;
                    if shared.stop.load(Ordering::Acquire) {
                        return;
                    }
                    if let Err(e) = tags.commit_tags().await {
                        *shared.failed.lock().unwrap() = Some(e.to_string());
                    }
                }
            }
        });
        Self { tags, shared, task }
    }

    /// Ask for a commit of everything buffered so far. Never waits.
    pub(crate) fn request(&self) {
        self.shared.wake.notify_one();
    }

    /// A commit that failed since the last call.
    pub(crate) fn take_error(&self) -> Result<()> {
        match self.shared.failed.lock().unwrap().take() {
            Some(e) => Err(crate::DooverError::Other(format!("tag commit failed: {e}"))),
            None => Ok(()),
        }
    }

    /// Stop, then commit what is still buffered: waits for a commit in
    /// flight, then makes one last one here.
    pub(crate) async fn finish(self) -> Result<()> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        let Self { tags, shared, task } = self;
        let _ = task.await;
        if let Some(e) = shared.failed.lock().unwrap().take() {
            return Err(crate::DooverError::Other(format!("tag commit failed: {e}")));
        }
        tags.commit_tags().await
    }
}
