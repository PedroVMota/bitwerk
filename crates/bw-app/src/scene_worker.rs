//! Background thread for the expensive scene work, so it never runs on the
//! UI thread:
//! * full scene builds (file load, new circuit, stress test, compaction);
//!   requests are coalesced and only the newest pending circuit is built;
//! * dropping retired scenes (freeing a 100k-object R-tree and its instance
//!   arrays takes milliseconds).
//!
//! Ordinary edits never come here: they patch the live scene in place.

use std::sync::Arc;
use std::thread::JoinHandle;

use bw_core::Circuit;
use bw_render::Scene;
use crossbeam_channel::{Receiver, Sender};

enum Job {
    Build(Arc<Circuit>, u64),
    Retire(Box<Scene>),
}

pub struct SceneWorker {
    tx: Option<Sender<Job>>,
    rx: Receiver<Box<Scene>>,
    thread: Option<JoinHandle<()>>,
}

impl SceneWorker {
    pub fn spawn(on_done: Box<dyn Fn() + Send>) -> std::io::Result<Self> {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<Job>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded();
        let thread = std::thread::Builder::new().name("bw-scene".into()).spawn(move || {
            while let Ok(job) = req_rx.recv() {
                let mut build = match job {
                    Job::Build(c, r) => Some((c, r)),
                    Job::Retire(old) => {
                        drop(old);
                        None
                    }
                };
                // Coalesce: drop retired scenes, keep only the newest build.
                while let Ok(next) = req_rx.try_recv() {
                    match next {
                        Job::Build(c, r) => build = Some((c, r)),
                        Job::Retire(old) => drop(old),
                    }
                }
                let Some((circuit, revision)) = build else { continue };
                let scene = Scene::build(&circuit, revision);
                drop(circuit);
                if res_tx.send(Box::new(scene)).is_err() {
                    return;
                }
                on_done();
            }
        })?;
        Ok(Self { tx: Some(req_tx), rx: res_rx, thread: Some(thread) })
    }

    pub fn request(&self, circuit: Arc<Circuit>, revision: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Build(circuit, revision));
        }
    }

    /// Frees a scene on the worker thread.
    pub fn retire(&self, scene: Box<Scene>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Retire(scene));
        }
    }

    /// Finished scenes, oldest first (non-blocking).
    pub fn poll(&self) -> impl Iterator<Item = Box<Scene>> + '_ {
        self.rx.try_iter()
    }
}

impl Drop for SceneWorker {
    fn drop(&mut self) {
        self.tx = None; // closes the channel → thread exits
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
