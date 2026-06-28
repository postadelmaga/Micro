//! Offload: a module pushes heavy CPU work onto the runtime's worker pool so its single
//! receive thread keeps draining the inbox — *receive fast, compute on the pool, publish
//! back*. These tests pin down the two properties that matter: the inbox keeps draining
//! while a slow job runs, and the pool shuts down deterministically (the process exits).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use framelite_bus::LocalBus;
use framelite_core::{Module, ModuleCtx, Runtime};
use framelite_protocol::{Channel, Envelope, ModuleId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Task {
    id: i64,
    slow: bool,
}

/// Result of handling a task: a fast `ack` (from the receive loop) or a `result` (from the
/// offloaded job). The `kind` lets the test tell the two apart on one channel.
#[derive(Serialize, Deserialize)]
struct Out {
    id: i64,
    kind: String,
}

/// Acks quick tasks inline; offloads slow tasks to the pool, which publishes the result
/// back on `out` when done. The receive loop never does the heavy work itself.
struct Worker;
impl Module for Worker {
    fn id(&self) -> ModuleId {
        ModuleId::new("worker")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("task")]
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(20)) {
                Ok(Some(env)) => {
                    let task: Task = match env.decode() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if task.slow {
                        // Receive fast: hand the heavy part to the pool and keep looping.
                        let bus = ctx.bus();
                        let me = ctx.id().clone();
                        ctx.offload(move || {
                            // A finite, bounded stand-in for heavy CPU work.
                            std::thread::sleep(Duration::from_millis(400));
                            let out = Out {
                                id: task.id,
                                kind: "result".into(),
                            };
                            let _ = bus.publish(Envelope::encode(me, "out", &out).unwrap());
                        });
                    } else {
                        ctx.publish_msg(
                            "out",
                            &Out {
                                id: task.id,
                                kind: "ack".into(),
                            },
                        )
                        .unwrap();
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

#[test]
fn offload_keeps_inbox_draining_and_publishes_result() {
    let bus = Arc::new(LocalBus::new());
    let mut rt = Runtime::with_bus(bus.clone());
    let out = bus.subscribe_with_capacity("out", 64);

    rt.spawn(Worker);

    // One slow task, then five quick ones right behind it.
    let main = ModuleId::new("main");
    bus.publish(Envelope::encode(main.clone(), "task", &Task { id: 0, slow: true }).unwrap())
        .unwrap();
    for id in 1..=5 {
        bus.publish(Envelope::encode(main.clone(), "task", &Task { id, slow: false }).unwrap())
            .unwrap();
    }

    // The five acks must land well before the 400ms slow job finishes: that is the proof
    // the receive loop kept draining instead of blocking inline on the heavy work.
    let start = Instant::now();
    let mut acks = Vec::new();
    for _ in 0..5 {
        let env = out
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("timed out waiting for an ack");
        let o: Out = env.decode().unwrap();
        assert_eq!(o.kind, "ack");
        acks.push(o.id);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "acks took {elapsed:?}; the slow job must have blocked the receive loop"
    );
    acks.sort();
    assert_eq!(acks, vec![1, 2, 3, 4, 5]);

    // The offloaded computation eventually publishes its result back on the bus.
    let env = out
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("timed out waiting for the offloaded result");
    let o: Out = env.decode().unwrap();
    assert_eq!(o.kind, "result");
    assert_eq!(o.id, 0);

    rt.shutdown();
    assert!(rt.join().is_clean());
}

/// Submits a batch of finite jobs, then returns. Used to check the pool drains queued work
/// and shuts down without hanging.
struct Batch {
    n: usize,
    done: Arc<AtomicUsize>,
}
impl Module for Batch {
    fn id(&self) -> ModuleId {
        ModuleId::new("batch")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for _ in 0..self.n {
            let done = self.done.clone();
            ctx.offload(move || {
                done.fetch_add(1, Ordering::Release);
            });
        }
    }
}

#[test]
fn pool_drains_queued_jobs_and_shuts_down_cleanly() {
    let done = Arc::new(AtomicUsize::new(0));
    let mut rt = Runtime::new();
    rt.spawn(Batch {
        n: 200,
        done: done.clone(),
    });

    // The module returns on its own; `join` then closes the pool and joins every worker.
    // If shutdown were not deterministic this call would hang and the test would never end.
    let report = rt.join();
    assert!(report.is_clean());
    // Every queued job ran before the workers exited.
    assert_eq!(done.load(Ordering::Acquire), 200);
}

/// Offloads a job that panics plus a job that records progress.
struct Panicky {
    done: Arc<AtomicUsize>,
}
impl Module for Panicky {
    fn id(&self) -> ModuleId {
        ModuleId::new("panicky")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        ctx.offload(|| panic!("boom inside a job"));
        let done = self.done.clone();
        ctx.offload(move || {
            done.fetch_add(1, Ordering::Release);
        });
    }
}

#[test]
fn a_panicking_job_does_not_poison_the_pool() {
    let done = Arc::new(AtomicUsize::new(0));
    let mut rt = Runtime::new();
    rt.spawn(Panicky { done: done.clone() });

    let report = rt.join();
    // A job panic is isolated to the worker (caught), not attributed to the module.
    assert!(report.is_clean(), "the module's run did not panic");
    // The pool kept working: the good job still ran.
    assert_eq!(done.load(Ordering::Acquire), 1);
}
