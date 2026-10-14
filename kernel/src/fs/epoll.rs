//! epoll instances (`epoll_create1`/`epoll_ctl`/`epoll_pwait`).
//!
//! An [`Epoll`] holds the interest list (items keyed by fd number and open
//! file description, like Linux's `(file*, fd)`) and a ready list of item
//! keys. Each item owns an [`EpHook`] registered on its file at
//! `EPOLL_CTL_ADD` (via `File::poll(Some(hook))`); the file's state
//! changes call `EpHook::notify`, which queues the item on the ready list
//! and wakes `epoll_wait`ers — Linux's `ep_poll_callback`.
//!
//! `wait` takes the ready list (under the epoll lock), polls each file with
//! **no** epoll lock held, then re-locks to commit: report the events that
//! intersect the item's mask, re-queue level-triggered items (they are
//! re-checked on the next wait), leave edge-triggered ones off until the
//! next notification, disable `EPOLLONESHOT` items. If nothing is ready it
//! blocks on the epoll's wait queue, re-checking the ready list under the
//! lock first, so a notification that raced with the polling is never
//! lost.
//!
//! Items hold a `Weak<OpenFile>`: when the last reference to the open file
//! description goes (every fd closed), the item is dropped the next time it
//! is looked at, which is Linux's "an fd is removed from all epoll sets
//! when its open file description is released" (its hook is gone with the
//! file, so no events can arrive for it meanwhile).
//!
//! Deviations: adding an epoll fd to another epoll (nesting) is refused
//! with `EINVAL` (Linux allows it; Go never does it); `EPOLLEXCLUSIVE` is
//! accepted and ignored (all waiters are woken); `EPOLLWAKEUP` ignored.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use patina_abi::current::{EpollEvent, Stat};
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::{EEXIST, EINTR, EINVAL, ENOENT, EPERM, Errno};
use patina_abi::fs::S_IFREG;

use super::file::{File, OpenFile, PollHook, PollList, alloc_ino, anon_stat};
use crate::sync::SpinLock;
use crate::task::WakeReason;
use crate::task::wait::{self, WaitQueue};

/// Item key: fd number and the open file description's address.
type Key = (i32, usize);

/// Flags that are not events (Linux `EP_PRIVATE_BITS`).
const PRIVATE: EpollFlags = EpollFlags::ET
    .union(EpollFlags::ONESHOT)
    .union(EpollFlags::WAKEUP)
    .union(EpollFlags::EXCLUSIVE);

struct Item {
    file: Weak<OpenFile>,
    /// Interest mask (always includes ERR|HUP while enabled) + flags.
    events: EpollFlags,
    data: u64,
    hook: Arc<EpHook>,
    /// On the ready list.
    queued: bool,
}

impl Item {
    fn enabled(&self) -> bool {
        !(self.events - PRIVATE).is_empty()
    }
}

struct Inner {
    items: BTreeMap<Key, Item>,
    ready: VecDeque<Key>,
    waiters: WaitQueue,
    /// Hooks watching this epoll fd itself (ppoll on an epoll fd).
    pollers: PollList,
}

fn waiters(i: &mut Inner) -> &mut WaitQueue {
    &mut i.waiters
}

impl Inner {
    fn enqueue(&mut self, key: Key) -> bool {
        match self.items.get_mut(&key) {
            Some(it) if !it.queued => {
                it.queued = true;
                self.ready.push_back(key);
                true
            }
            _ => false,
        }
    }
}

pub struct Epoll {
    inner: SpinLock<Inner>,
    me: Weak<Epoll>,
    ino: u64,
}

/// An item's registration on its file.
pub struct EpHook {
    ep: Weak<Epoll>,
    key: Key,
}

impl PollHook for EpHook {
    fn notify(&self, events: EpollFlags) {
        if let Some(ep) = self.ep.upgrade() {
            ep.item_ready(self, events);
        }
    }
}

/// Result of polling one snapshotted item without the epoll lock.
struct Polled {
    key: Key,
    hook: Arc<EpHook>,
    /// `None`: the open file description is gone.
    ev: Option<EpollFlags>,
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        Arc::new_cyclic(|me| Epoll {
            inner: SpinLock::new(Inner {
                items: BTreeMap::new(),
                ready: VecDeque::new(),
                waiters: WaitQueue::new(),
                pollers: PollList::new(),
            }),
            me: me.clone(),
            ino: alloc_ino(),
        })
    }

    /// `ep_poll_callback`: the item of `hook` may be ready.
    fn item_ready(&self, hook: &EpHook, events: EpollFlags) {
        let n = {
            let mut g = self.inner.lock();
            let Some(it) = g.items.get(&hook.key) else {
                return;
            };
            if !core::ptr::eq(Arc::as_ptr(&it.hook), hook)
                || !it.enabled()
                || (!events.is_empty() && !events.intersects(it.events - PRIVATE))
            {
                return;
            }
            if !g.enqueue(hook.key) {
                return;
            }
            g.waiters.wake_all();
            g.pollers.collect(EpollFlags::IN | EpollFlags::RDNORM)
        };
        n.fire();
    }

    /// Drop items whose open file description is gone.
    fn prune(g: &mut Inner) {
        g.items.retain(|_, it| it.file.strong_count() > 0);
    }

    /// `EPOLL_CTL_ADD`.
    pub fn add(
        &self,
        fd: i32,
        of: &Arc<OpenFile>,
        events: EpollFlags,
        data: u64,
    ) -> Result<(), Errno> {
        if of.file.as_epoll().is_some() {
            return Err(Errno(EINVAL)); // nesting / self-add
        }
        if !of.file.pollable() {
            return Err(Errno(EPERM));
        }
        let key = (fd, Arc::as_ptr(of) as usize);
        let hook = Arc::new(EpHook {
            ep: self.me.clone(),
            key,
        });
        let mask = events | EpollFlags::ERR | EpollFlags::HUP;
        {
            let mut g = self.inner.lock();
            Self::prune(&mut g);
            if g.items.contains_key(&key) {
                return Err(Errno(EEXIST));
            }
            g.items.insert(
                key,
                Item {
                    file: Arc::downgrade(of),
                    events: mask,
                    data,
                    hook: hook.clone(),
                    queued: false,
                },
            );
        }
        // Register after inserting, so a notification between the two is
        // not dropped; queue at once if already ready.
        let dyn_hook: Arc<dyn PollHook> = hook.clone();
        let ev = of.file.poll(Some(&dyn_hook));
        if ev.intersects(mask - PRIVATE) {
            self.item_ready(&hook, ev);
        }
        Ok(())
    }

    /// `EPOLL_CTL_MOD`.
    pub fn modify(
        &self,
        fd: i32,
        of: &Arc<OpenFile>,
        events: EpollFlags,
        data: u64,
    ) -> Result<(), Errno> {
        let key = (fd, Arc::as_ptr(of) as usize);
        let mask = events | EpollFlags::ERR | EpollFlags::HUP;
        let hook = {
            let mut g = self.inner.lock();
            let it = g.items.get_mut(&key).ok_or(Errno(ENOENT))?;
            if it.events.contains(EpollFlags::EXCLUSIVE) {
                return Err(Errno(EINVAL));
            }
            it.events = mask;
            it.data = data;
            it.hook.clone()
        };
        let ev = of.file.poll(None);
        if ev.intersects(mask - PRIVATE) {
            self.item_ready(&hook, ev);
        }
        Ok(())
    }

    /// `EPOLL_CTL_DEL`.
    pub fn delete(&self, fd: i32, of: &Arc<OpenFile>) -> Result<(), Errno> {
        let key = (fd, Arc::as_ptr(of) as usize);
        let it = {
            let mut g = self.inner.lock();
            let it = g.items.remove(&key).ok_or(Errno(ENOENT))?;
            g.ready.retain(|k| *k != key);
            it
        };
        drop(it);
        Ok(())
    }

    /// Collect up to `max` events. With `block`, waits until at least one
    /// is available or `deadline` (monotonic ns; `None` = forever) passes;
    /// without, returns at once (`timeout == 0`). The wait is
    /// interruptible: a pending signal ends it with `EINTR` (never
    /// restarted, as Linux's `ep_poll`).
    pub fn wait(
        &self,
        max: usize,
        block: bool,
        deadline: Option<u64>,
    ) -> Result<Vec<EpollEvent>, Errno> {
        let mut out = Vec::new();
        let mut may_block = block;
        loop {
            // 1. Snapshot the ready list.
            let snap: Vec<(Key, Weak<OpenFile>, Arc<EpHook>)> = {
                let mut g = self.inner.lock();
                let keys: Vec<Key> = g.ready.drain(..).collect();
                keys.into_iter()
                    .filter_map(|k| {
                        let it = g.items.get_mut(&k)?;
                        it.queued = false;
                        Some((k, it.file.clone(), it.hook.clone()))
                    })
                    .collect()
            };
            // 2. Poll with no epoll lock held (a dropped upgrade may run
            //    File::release, which notifies hooks, including ours).
            let polled: Vec<Polled> = snap
                .into_iter()
                .map(|(key, file, hook)| Polled {
                    key,
                    hook,
                    ev: file.upgrade().map(|f| f.file.poll(None)),
                })
                .collect();
            // 3. Commit.
            let mut g = self.inner.lock();
            let mut dead = Vec::new();
            for p in polled {
                let Some(it) = g.items.get_mut(&p.key) else {
                    continue;
                };
                if !Arc::ptr_eq(&it.hook, &p.hook) {
                    continue;
                }
                let Some(ev) = p.ev else {
                    dead.push(p.key);
                    continue;
                };
                let rep = ev & (it.events - PRIVATE);
                if rep.is_empty() {
                    continue;
                }
                if out.len() >= max {
                    g.enqueue(p.key);
                    continue;
                }
                out.push(EpollEvent::new(rep.bits(), it.data));
                if it.events.contains(EpollFlags::ONESHOT) {
                    it.events &= PRIVATE;
                } else if !it.events.contains(EpollFlags::ET) {
                    g.enqueue(p.key);
                }
            }
            let dead_items: Vec<Item> = dead.iter().filter_map(|k| g.items.remove(k)).collect();
            if !out.is_empty() || !may_block {
                drop(g);
                drop(dead_items);
                return Ok(out);
            }
            if !g.ready.is_empty() {
                // Something was queued while we polled: look again.
                drop(g);
                drop(dead_items);
                continue;
            }
            if deadline.is_some_and(|d| crate::arch::timer::now_ns() >= d) {
                return Ok(out);
            }
            // `dead_items` holds no file reference; dropping it under the
            // lock is fine, but keep it simple and drop first.
            drop(dead_items);
            match wait::block_interruptible(g, waiters, 0, 0, deadline) {
                // One last non-blocking look (Linux re-checks too).
                WakeReason::TimedOut => may_block = false,
                WakeReason::Interrupted => return Err(Errno(EINTR)),
                WakeReason::Woken => {}
            }
        }
    }
}

impl File for Epoll {
    fn kind(&self) -> &'static str {
        "epoll"
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let mut g = self.inner.lock();
        if let Some(h) = hook {
            g.pollers.add(h);
        }
        // Approximate: a queued item may turn out not to be ready on the
        // next wait (Linux re-polls the items here).
        if g.ready.is_empty() {
            EpollFlags::empty()
        } else {
            EpollFlags::IN | EpollFlags::RDNORM
        }
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFREG | 0o600, self.ino))
    }

    fn as_epoll(&self) -> Option<&Epoll> {
        Some(self)
    }
}
