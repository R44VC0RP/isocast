//! In-process Windows GPU ownership. A receipt follows the last actual owner,
//! including COM objects and callbacks, not the lifetime of an admission plan.
use anyhow::{Result, bail, ensure};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CLEANUP: &str = "idle cache retirement";
const TAKE: &str = "active take";

pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

struct Epoch {
    number: u64,
    accepting: AtomicBool,
    counts: Mutex<BTreeMap<&'static str, Count>>,
    released: Condvar,
}

#[derive(Default)]
struct Count {
    live: usize,
    quarantined: bool,
}

impl Epoch {
    fn new(number: u64, accepting: bool) -> Arc<Self> {
        Arc::new(Self {
            number,
            accepting: AtomicBool::new(accepting),
            counts: Mutex::default(),
            released: Condvar::new(),
        })
    }
    fn track(self: &Arc<Self>, kind: &'static str) -> Owner {
        self.counts.lock().unwrap().entry(kind).or_default().live += 1;
        Owner(Arc::new(Receipt {
            epoch: self.clone(),
            kind,
        }))
    }
}

#[derive(Clone)]
pub struct Owner(Arc<Receipt>);
/// Only supplied inside the post-barrier open callback.
pub struct Replacement {
    owner: Owner,
}
/// Proof that both qualified caches were installed under their cache locks.
pub struct Published(());
impl Replacement {
    pub fn owner(&self, kind: &'static str) -> Owner {
        self.owner.child(kind)
    }
    pub fn publish(self, install: impl FnOnce()) -> Published {
        self.owner.0.epoch.accepting.store(true, Ordering::Release);
        install();
        Published(())
    }
}
struct Receipt {
    epoch: Arc<Epoch>,
    kind: &'static str,
}

impl Owner {
    /// The corresponding native owner is deliberately retained until exit.
    /// Its registration must never suggest that waiting or retrying is safe.
    pub fn quarantine(self) {
        let epoch = &self.0.epoch;
        epoch
            .counts
            .lock()
            .unwrap()
            .get_mut(self.0.kind)
            .expect("registered owner")
            .quarantined = true;
        epoch.released.notify_all();
        std::mem::forget(self);
    }
    pub fn number(&self) -> u64 {
        self.0.epoch.number
    }
    /// Deriving a receipt never makes work admissible. Even if freeze races a
    /// native open, the parent is still counted, so zero cannot be observed.
    pub fn child(&self, kind: &'static str) -> Self {
        self.0.epoch.track(kind)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            self.0.epoch.accepting.load(Ordering::Acquire),
            "{} belongs to retired GPU generation {}",
            self.0.kind,
            self.number()
        );
        Ok(())
    }
    pub fn check_peer(&self, peer: &Self) -> Result<()> {
        self.check()?;
        ensure!(
            Arc::ptr_eq(&self.0.epoch, &peer.0.epoch),
            "GPU generation mismatch: expected {}, frame is {}",
            self.number(),
            peer.number()
        );
        Ok(())
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        let mut counts = self.epoch.counts.lock().unwrap();
        let count = counts.get_mut(self.kind).expect("registered owner");
        count.live -= 1;
        if count.live == 0 {
            counts.remove(self.kind);
        }
        if counts.is_empty() {
            self.epoch.released.notify_all();
        }
    }
}

pub struct Registry {
    current: Mutex<Arc<Epoch>>,
    replacing: Mutex<()>,
    managed: AtomicBool,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            current: Mutex::new(Epoch::new(1, true)),
            replacing: Mutex::new(()),
            managed: AtomicBool::new(false),
        }
    }
}
impl Registry {
    pub fn take(&self) -> Result<Owner> {
        self.register(TAKE)
    }
    /// Diagnostic snapshot only. Only `replace` can freeze admission and
    /// prove that an empty list will stay empty.
    pub fn owners(&self) -> (u64, BTreeMap<&'static str, usize>) {
        let current = self.current.lock().unwrap();
        (
            current.number,
            current
                .counts
                .lock()
                .unwrap()
                .iter()
                .map(|(&kind, count)| (kind, count.live))
                .collect(),
        )
    }
    pub fn manage_idle_replacement(&self) {
        self.managed.store(true, Ordering::Release);
    }
    pub fn managed(&self) -> bool {
        self.managed.load(Ordering::Acquire)
    }
    pub fn retiring(&self) -> bool {
        !self
            .current
            .lock()
            .unwrap()
            .accepting
            .load(Ordering::Acquire)
    }
    pub fn register(&self, kind: &'static str) -> Result<Owner> {
        let epoch = self.current.lock().unwrap();
        ensure!(
            epoch.accepting.load(Ordering::Acquire),
            "GPU generation {} closed to new {kind} owners",
            epoch.number
        );
        Ok(epoch.track(kind))
    }
    /// Freeze root admission, release caches, then wait WITHOUT a device or
    /// registry lock held by the opener. Timeout leaves admission closed: an
    /// uncertain or leaked owner cannot disappear by spending a retry ticket.
    pub fn replace<G>(
        &self,
        timeout: Duration,
        check_idle: impl FnOnce() -> Result<G>,
        release: impl FnOnce(Owner) -> Result<()>,
        open: impl FnOnce(Replacement) -> Result<Published>,
    ) -> Result<u64> {
        self.replace_generation(timeout, check_idle, &mut None, release, open)
    }

    /// The recorder, not an idle Start or encoder, supplies its own receipt.
    /// Transfer that exclusion across the barrier without counting it as a
    /// native owner or ever admitting two takes on the replacement.
    pub fn replace_generation<G>(
        &self,
        timeout: Duration,
        check: impl FnOnce() -> Result<G>,
        take: &mut Option<Owner>,
        release: impl FnOnce(Owner) -> Result<()>,
        open: impl FnOnce(Replacement) -> Result<Published>,
    ) -> Result<u64> {
        let recording = take.is_some();
        let _one = self
            .replacing
            .try_lock()
            .map_err(|_| anyhow::anyhow!("GPU replacement is already in progress"))?;
        let old = {
            // Keep the daemon's live-output state locked through the freeze,
            // but never wait for teardown while holding that state lock.
            let _idle_guard = check()?;
            let current = self.current.lock().unwrap();
            if let Some(receipt) = take.as_ref() {
                ensure!(
                    receipt.0.kind == TAKE && Arc::ptr_eq(&receipt.0.epoch, &current),
                    "stale recording GPU authority"
                );
                ensure!(
                    current
                        .counts
                        .lock()
                        .unwrap()
                        .get(TAKE)
                        .is_some_and(|c| c.live == 1)
                        && Arc::strong_count(&receipt.0) == 1,
                    "another active take still owns this generation"
                );
                drop(take.take());
            }
            ensure!(
                !current.counts.lock().unwrap().contains_key(TAKE),
                "cannot replace the GPU during an active take"
            );
            current.accepting.store(false, Ordering::Release);
            current.clone()
        };
        let until = Instant::now() + timeout;
        // The receipt follows cache teardown through its final native Release.
        // Don't start another teardown thread if the first one is still stuck.
        if !old.counts.lock().unwrap().contains_key(CLEANUP) {
            release(old.track(CLEANUP))?;
        }
        let counts = old.counts.lock().unwrap();
        let (counts, _) = old
            .released
            .wait_timeout_while(
                counts,
                until.saturating_duration_since(Instant::now()),
                |c| !c.is_empty() && !c.values().any(|v| v.quarantined),
            )
            .unwrap();
        if !counts.is_empty() {
            let owners = counts
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{k} ({}){}",
                        v.live,
                        if v.quarantined { " [quarantined]" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let remedy = if counts.values().any(|v| v.quarantined) {
                "restart Isocast"
            } else {
                "release these owners and retry"
            };
            bail!(
                "GPU generation {} did not retire: {owners}; {remedy}",
                old.number
            );
        }
        drop(counts);
        let next = Epoch::new(
            old.number
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("GPU generation overflow"))?,
            false,
        );
        *self.current.lock().unwrap() = next.clone();
        if recording {
            *take = Some(next.track(TAKE));
        }
        open(Replacement {
            owner: next.track("replacement"),
        })?; // a failed candidate stays closed
        Ok(next.number)
    }

    pub fn quarantine_take(&self, take: &Owner) -> Result<()> {
        let current = self.current.lock().unwrap();
        ensure!(
            take.0.kind == TAKE && Arc::ptr_eq(&current, &take.0.epoch),
            "stale recording GPU authority"
        );
        current.accepting.store(false, Ordering::Release);
        current.track("repeated device loss").quarantine();
        Ok(())
    }
}

#[cfg(windows)]
#[windows::core::implement()]
struct NativeReceipt {
    _owner: Owner,
}
#[cfg(windows)]
impl Owner {
    /// The runtime releases this IUnknown when the native object goes away,
    /// even if a driver, HAL object or unmanaged COM clone outlives Rust.
    pub fn native_receipt(&self) -> windows::core::IUnknown {
        NativeReceipt {
            _owner: self.clone(),
        }
        .into()
    }
}
#[cfg(windows)]
pub const RECEIPT_ID: windows::core::GUID =
    windows::core::GUID::from_u128(0xa2581919_71b6_4ab9_964f_bee5afa21905);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_current_take_can_replace_and_quarantine_its_successor() {
        let r = Registry::default();
        let mut take = Some(r.take().unwrap());
        let other = r.take().unwrap();
        assert!(
            r.replace_generation(
                Duration::ZERO,
                || Ok(()),
                &mut take,
                |_| unreachable!(),
                |_| unreachable!()
            )
            .is_err()
        );
        take.as_ref().unwrap().check().unwrap();
        drop(other);
        r.replace_generation(
            Duration::ZERO,
            || Ok(()),
            &mut take,
            |_| Ok(()),
            |c| Ok(c.publish(|| {})),
        )
        .unwrap();
        let take = take.unwrap();
        assert_eq!(take.number(), 2);
        assert!(
            r.replace(
                Duration::ZERO,
                || Ok(()),
                |_| unreachable!(),
                |_| unreachable!()
            )
            .is_err()
        );
        r.quarantine_take(&take).unwrap();
        drop(take);
        assert!(r.take().is_err());
        let err = r
            .replace(Duration::ZERO, || Ok(()), |_| Ok(()), |_| unreachable!())
            .unwrap_err();
        assert!(err.to_string().contains("restart Isocast"));
    }
    #[test]
    fn live_client_refusal_preserves_admission_without_teardown() {
        let r = Registry::default();
        let preview = r.register("preview").unwrap();
        let err = r
            .replace::<()>(
                Duration::ZERO,
                || bail!("stop the preview first"),
                |_| panic!("retired an active preview"),
                |_| unreachable!(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("preview"));
        preview.check().unwrap();
        assert_eq!(r.register("stream").unwrap().number(), preview.number());
    }
    #[test]
    fn only_permanently_retained_owners_require_a_restart() {
        let r = Registry::default();
        let guard = r.register("MF converter").unwrap();
        let attempt = || {
            r.replace(Duration::ZERO, || Ok(()), |_| Ok(()), |_| unreachable!())
                .unwrap_err()
                .to_string()
        };
        let busy = attempt();
        assert!(busy.contains("release these owners and retry") && !busy.contains("quarantined"));
        guard.quarantine();
        let permanent = attempt();
        assert!(
            permanent.contains("MF converter (1) [quarantined]")
                && permanent.contains("restart Isocast")
        );
    }
    #[test]
    fn active_take_forbids_freeze_and_a_draining_owner_wakes_replacement() {
        let r = Registry::default();
        let take = r.take().unwrap();
        assert!(
            r.replace(
                Duration::ZERO,
                || Ok(()),
                |_| panic!("froze during take"),
                |_| unreachable!()
            )
            .is_err()
        );
        take.check().unwrap();
        drop(take);
        let pool = r.register("pool").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                rx.recv().unwrap();
                assert!(pool.check().is_err());
                drop(pool);
            });
            assert_eq!(
                r.replace(
                    Duration::from_secs(2),
                    || Ok(()),
                    |_| {
                        tx.send(()).unwrap();
                        Ok(())
                    },
                    |candidate| Ok(candidate.publish(|| {}))
                )
                .unwrap(),
                2
            );
        });
    }
    #[test]
    fn retry_cannot_spawn_more_teardown_or_admit_a_failed_candidate() {
        let r = Registry::default();
        let mut teardown = None;
        assert!(
            r.replace(
                Duration::ZERO,
                || Ok(()),
                |owner| {
                    teardown = Some(owner);
                    Ok(())
                },
                |_| unreachable!()
            )
            .is_err()
        );
        let e = r
            .replace(
                Duration::ZERO,
                || Ok(()),
                |_| panic!("duplicate teardown"),
                |_| unreachable!(),
            )
            .unwrap_err();
        assert!(e.to_string().contains(CLEANUP));
        drop(teardown);
        let mut late = None;
        assert!(
            r.replace(
                Duration::ZERO,
                || Ok(()),
                |_| Ok(()),
                |candidate| {
                    late = Some(candidate.owner("failed native open"));
                    bail!("probe failed")
                }
            )
            .is_err()
        );
        assert!(r.register("new writer").is_err());
        assert!(
            r.replace(Duration::ZERO, || Ok(()), |_| Ok(()), |_| unreachable!())
                .unwrap_err()
                .to_string()
                .contains("failed native open")
        );
        drop(late);
    }
    #[test]
    fn blocked_generation_names_last_owner_and_cannot_be_reopened_by_timeout() {
        let r = Registry::default();
        let frame = r.register("texture").unwrap();
        let held = frame.clone();
        drop(frame);
        let e = r
            .replace(
                Duration::ZERO,
                || Ok(()),
                |_| Ok(()),
                |_| panic!("opened early"),
            )
            .unwrap_err();
        assert!(e.to_string().contains("texture (1)"));
        assert!(held.check().is_err() && r.register("pool").is_err());
        assert_eq!(
            r.replace(
                Duration::ZERO,
                || Ok(()),
                |_| {
                    drop(held);
                    Ok(())
                },
                |candidate| {
                    assert!(candidate.owner.check().is_err());
                    let cached = candidate.owner("test device");
                    Ok(candidate.publish(|| {
                        cached.check().unwrap();
                        r.register("cache reader").unwrap();
                    }))
                }
            )
            .unwrap(),
            2
        );
        let next = r.register("frame").unwrap();
        assert_eq!(next.number(), 2);
        assert!(next.check_peer(&next.child("importer")).is_ok());
        assert!(
            next.check_peer(&Registry::default().register("foreign").unwrap())
                .is_err()
        );
    }
}
