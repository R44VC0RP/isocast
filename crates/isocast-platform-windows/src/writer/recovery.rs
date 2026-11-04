//! The movie, timestamp FIFO and AAC tracks never leave the file worker.
//! Only the retired native session and the recipe for its successor cross
//! to the open worker. A late result has no way to publish a packet.

use std::fmt;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::*;
use crate::platform::RecordingReopen;
use isocast_engine::record::recovery::{Attempt, OutputRecovery, RetryRefused};

#[derive(Debug)]
pub(super) struct CodecFault;
impl fmt::Display for CodecFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native recording encoder failed")
    }
}

// Never turn a driver that won't return from teardown into unbounded detached
// jobs across successive takes. Workers retain their permit until native Drop
// finishes, even when their file already returned or cancelled its receiver.
static RECOVERING: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn acquire() -> Result<Arc<Self>> {
        RECOVERING
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 8).then_some(n + 1)
            })
            .map_err(|_| {
                anyhow!("eight encoder recoveries still own native resources; cannot safely reopen")
            })?;
        Ok(Arc::new(Self))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        RECOVERING.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(super) struct Recovery {
    pub recipe: Option<RecordingReopen>,
    pub context: Option<OutputRecovery>,
    pub incident: Option<u64>,
    attempt: Option<Attempt>,
    opening: Option<Receiver<Result<WindowsVideoEncoder>>>,
    retired: Option<WindowsVideoEncoder>,
    permit: Option<Arc<Permit>>,
    cancel: Arc<AtomicBool>,
    last_error: String,
    pub priority: Option<isocast_protocol::CanvasId>,
    pub intentional_drops: u64,
    #[cfg(debug_assertions)]
    inject: Option<(bool, u32)>,
    #[cfg(debug_assertions)]
    armed: bool,
    #[cfg(debug_assertions)]
    hold_admission: bool,
    #[cfg(debug_assertions)]
    test_lease: Option<crate::nvenc::RecoveryAdmission>,
}

#[derive(Default)]
pub(super) struct LossCounts {
    reported_native: u64,
    abandoned: u64,
}

impl LossCounts {
    pub fn take(&mut self, native: u64) -> (u64, u64) {
        let delta = native.saturating_sub(self.reported_native);
        self.reported_native = native;
        (std::mem::take(&mut self.abandoned), delta)
    }
    fn abandon(&mut self, originals: u64, native: u64) {
        // Native losses overlap this generation's unresolved FIFO. Don't count
        // a reported loss again when its timestamp is finally abandoned.
        self.abandoned += originals.max(native).saturating_sub(self.reported_native);
        self.reported_native = 0;
    }
}

fn compatible(frozen: &[u8], candidate: &[u8]) -> Result<()> {
    anyhow::ensure!(
        frozen == candidate,
        "replacement decoder configuration differs from the fixed movie sample entry; ended early (a new file is required)"
    );
    Ok(())
}

/// Verify the actual first AU, even if the driver or transport marked it
/// clean. Mixed IDR/inter slices and open-GOP HEVC CRA are not a boundary.
fn closed_idr(codec: VideoCodec, mut au: &[u8]) -> bool {
    let mut found = false;
    while !au.is_empty() {
        let Some((n, rest)) = au.split_first_chunk::<4>() else {
            return false;
        };
        let Some((nal, tail)) = rest.split_at_checked(u32::from_be_bytes(*n) as usize) else {
            return false;
        };
        let Some(&first) = nal.first() else {
            return false;
        };
        let (vcl, idr) = match codec {
            VideoCodec::H264 => {
                let kind = first & 31;
                ((1..=5).contains(&kind), kind == 5)
            }
            VideoCodec::Hevc => {
                let kind = (first >> 1) & 63;
                (kind <= 31, matches!(kind, 19 | 20))
            }
        };
        if vcl && !idr {
            return false;
        }
        found |= idr;
        au = tail;
    }
    found
}

impl VideoWriter {
    pub(crate) fn allow_reopen(&mut self, recipe: RecordingReopen) {
        #[cfg(debug_assertions)]
        if std::env::var_os("ISOCAST_SOCKET").is_some()
            && let Ok(value) = std::env::var("ISOCAST_TEST_ENCODER_FAILURE")
            && let Some(setup) = std::env::var_os("ISOCAST_SETUP")
            && std::path::Path::new(&setup)
                .parent()
                .is_some_and(|root| root.is_absolute() && self.spec.path.starts_with(root))
        {
            let fields: Vec<_> = value.split(':').collect();
            if let [
                owner,
                operation @ ("input" | "output" | "busy_input"),
                count,
            ] = fields.as_slice()
                && *owner == recipe.output
                && let Ok(count @ 1..=4) = count.parse::<u32>()
            {
                self.recovery.inject = Some((*operation == "output", count));
                self.recovery.hold_admission = *operation == "busy_input";
            }
        }
        self.recovery.recipe = Some(recipe);
    }

    #[cfg(debug_assertions)]
    pub(super) fn inject_fault(&mut self) -> Result<()> {
        if self.recovery.armed || !isocast_diagnostics::developer() || self.last_written.is_none() {
            return Ok(());
        }
        if let Some((output, left)) = &mut self.recovery.inject
            && *left > 0
        {
            self.encoder
                .as_mut()
                .expect("live encoder")
                .inject_failure(*output)?;
            *left -= 1;
            self.recovery.armed = true;
        }
        Ok(())
    }

    pub(super) fn abandon_pending(&mut self) {
        // These were counted on admission by the engine. Retire the old FIFO
        // as a unit; never match its timestamps to packets of the new session.
        let originals = self.pending.iter().filter(|p| !p.repeated).count() as u64;
        self.losses.abandon(originals, self.native_losses());
        self.accepted_originals -= originals;
        self.accepted -= self.pending.len() as u64;
        self.pending.clear();
    }

    pub(super) fn handle_codec_fault(&mut self, error: anyhow::Error) -> Result<()> {
        let Some(context) = self.recovery.context.clone() else {
            return Err(error);
        };
        if self.closed || self.output_failed || self.last_written.is_none() {
            return Err(error);
        }
        self.recovery
            .recipe
            .as_ref()
            .expect("configured")
            .healthy_device()?;
        if self.recovery.incident.is_none() {
            // Keep the ready prefix if the submit failed after prior pictures
            // completed. No EOS/barrier on a broken session; queued dependent
            // pictures beyond an unvalidated AU are never published.
            for _ in 0..MAX_PENDING {
                let Some(packet) = self
                    .encoder
                    .as_mut()
                    .and_then(|e| e.receive().ok().flatten())
                else {
                    break;
                };
                self.collect(packet)?;
            }
            let first =
                self.time_at(self.last_written.expect("known prefix") + u64::from(FRAME_DURATION))?;
            let why = format!(
                "{}: {error:#}",
                self.encoder.as_ref().expect("failed encoder").backend()
            );
            self.recovery.incident = Some(
                context
                    .interrupted(first, crate::now(), &why)
                    .context("video recovery history full; cannot reopen encoder")?,
            );
            self.recovery.permit = Some(Permit::acquire()?);
        }
        if let Some(ticket) = self.recovery.attempt.take() {
            context.failed_attempt(ticket, crate::now());
        }
        self.recovery.last_error = format!("{error:#}");
        self.abandon_pending();
        self.recovery.retired = self.encoder.take();
        // Includes the last held, validated picture, not abandoned inputs.
        self.commit_packets()?;
        self.poll_reopen()?;
        Ok(())
    }

    /// Never wait for teardown/preflight here: this same queue carries PCM.
    pub(super) fn poll_reopen(&mut self) -> Result<bool> {
        let Some(id) = self.recovery.incident else {
            return Ok(true);
        };
        let context = self.recovery.context.as_ref().expect("configured");
        let now = crate::now();
        if let Some(ticket) = self.recovery.attempt
            && now >= ticket.deadline
        {
            // A native call can outlive its deadline. Invalidate its single
            // receiver instead of starting another concurrent native job.
            bail!(
                "encoder recovery timed out before a verified IDR; last error: {}",
                self.recovery.last_error
            );
        }
        if let Some(rx) = &self.recovery.opening {
            let result = match rx.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return Ok(false),
                Err(TryRecvError::Disconnected) => Err(anyhow!("encoder reopen worker exited")),
            };
            self.recovery.opening = None;
            match result {
                Ok(encoder) => {
                    self.encoder = Some(encoder);
                    let encoder = self.encoder.as_mut().expect("installed candidate");
                    let candidate = encoder.decoder_config().and_then(|description| {
                        compatible(
                            self.decoder_config
                                .as_deref()
                                .context("no frozen description")?,
                            &description,
                        )
                    });
                    if let Err(error) = candidate {
                        // Not a usable generation. Retire it (and its budget)
                        // off-thread now, not when the take eventually stops.
                        self.retire_without_reopen();
                        return Err(error);
                    }
                    if let Some(canvas) = self.recovery.priority {
                        encoder.prioritize_video(canvas);
                    }
                    #[cfg(debug_assertions)]
                    {
                        self.recovery.armed = false;
                    }
                }
                Err(e) => {
                    context
                        .failed_attempt(self.recovery.attempt.take().expect("opening ticket"), now);
                    self.recovery.last_error = format!("{e:#}");
                }
            }
        }
        if self.encoder.is_some() {
            return Ok(true);
        }
        // Isolated split/contended-admission e2e: hold the *real* lease
        // until debug_set(false). No driver fault or timing race is needed.
        #[cfg(debug_assertions)]
        if self.recovery.hold_admission {
            if !isocast_diagnostics::developer() {
                self.recovery.test_lease = None;
            } else if self.recovery.test_lease.is_none() {
                self.recovery.test_lease = crate::nvenc::RecoveryAdmission::try_acquire();
            }
        }
        let (ticket, lease) =
            match context.attempt_when(id, now, crate::nvenc::RecoveryAdmission::try_acquire) {
                Ok(Some(reserved)) => reserved,
                Ok(None) | Err(RetryRefused::WaitUntil(_)) => return Ok(false),
                Err(why) => bail!(
                    "encoder recovery ended ({why:?}): {}",
                    self.recovery.last_error
                ),
            };
        // Rendezvous: dropping the receiver on Stop never has to destroy a
        // queued native encoder on the file finalizer.
        let (tx, rx) = mpsc::sync_channel(0);
        let old = self.recovery.retired.take();
        let recipe = self.recovery.recipe.clone().expect("configured");
        let permit = self.recovery.permit.clone().expect("recovery permit");
        let cancel = self.recovery.cancel.clone();
        std::thread::Builder::new()
            .name("recording encoder reopen".into())
            .spawn(move || {
                let _permit = permit;
                if let Some(old) = old {
                    old.retire_failed();
                } // includes reservation/teardown/quarantine
                let result = (|| {
                    anyhow::ensure!(
                        !cancel.load(Ordering::Acquire) && crate::now() < ticket.deadline,
                        "encoder retirement exceeded recovery deadline"
                    );
                    let _apartment = crate::native::Apartment::new()?;
                    recipe.open(lease)
                })();
                let _ = tx.send(result);
            })
            .context("starting encoder reopen worker")?;
        self.recovery.opening = Some(rx);
        self.recovery.attempt = Some(ticket);
        Ok(false)
    }

    pub(super) fn check_boundary(&self, packet: &EncodedVideo) -> Result<Option<(Attempt, Time)>> {
        let Some(ticket) = self.recovery.attempt else {
            return Ok(None);
        };
        let now = crate::now();
        anyhow::ensure!(
            self.recovery
                .context
                .as_ref()
                .expect("configured")
                .can_resume(ticket, packet.pts, now),
            "recovery expired before the first picture"
        );
        // packet.keyframe is parsed from the AU by h264::packet/hevc::packet
        // (AVC 5, HEVC 19/20), not an MF clean-point flag or FORCEIDR request.
        anyhow::ensure!(
            packet.keyframe && closed_idr(self.codec, &packet.data),
            "replacement's first real picture is not a closed IDR; prefix retained"
        );
        Ok(Some((ticket, now)))
    }

    pub(super) fn publish_resume(&mut self, ticket: Attempt, pts: Time, checked_at: Time) {
        let encoder = self.encoder.as_ref().expect("verified encoder");
        let backend = format!(
            "{} ({:?}); {}",
            encoder.backend(),
            encoder.recording_quality(),
            encoder.fallback_reasons().join("; ")
        );
        self.recovery.context.as_ref().expect("configured").resumed(
            ticket,
            pts,
            checked_at,
            &backend,
            !matches!(
                encoder.backend(),
                crate::EncoderBackend::MediaFoundationSoftware(_)
            ),
        );
        self.recovery.incident = None;
        self.recovery.permit = None;
        self.recovery.attempt = None;
        self.recovery.last_error.clear();
    }

    pub(super) fn end_incident(&mut self, error: &anyhow::Error) {
        if let Some(id) = self.recovery.incident {
            let context = self.recovery.context.as_ref().expect("configured");
            let why = format!("{error:#}");
            if self
                .mux
                .as_ref()
                .is_some_and(|m| m.progress().committed.is_none())
            {
                context.storage_failed(id, crate::now(), &why);
            } else {
                context.ended_early(id, crate::now(), &why);
            }
        }
    }

    pub(super) fn retire_without_reopen(&mut self) {
        self.recovery.cancel.store(true, Ordering::Release);
        self.recovery.opening.take(); // disconnect; a late result owns no mux
        if self.encoder.is_some() {
            self.recovery.retired = self.encoder.take();
        }
        if let Some(permit) = self.recovery.permit.take()
            && let Some(old) = self.recovery.retired.take()
        {
            std::thread::spawn(move || {
                let _permit = permit;
                old.retire_failed()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_losses_survive_a_failed_recovery_without_double_counting_retirement() {
        let mut losses = LossCounts::default();
        assert_eq!(losses.take(1), (0, 1)); // reported even if we can't enter recovery
        assert_eq!(losses.take(1), (0, 0));
        losses.abandon(4, 2); // two native losses, one already reported, within four originals
        assert_eq!(losses.take(0), (3, 0));
        assert_eq!(losses.take(1), (0, 1)); // new generation's first loss
    }
    #[test]
    fn a_single_changed_parameter_set_requires_a_new_file() {
        assert!(compatible(&[1, 100, 0, 41], &[1, 100, 0, 41]).is_ok());
        assert!(compatible(&[1, 100, 0, 41], &[1, 100, 0, 42]).is_err());
    }

    #[test]
    fn closed_idr_checks_all_slices_of_the_returned_access_unit() {
        let avc = [0, 0, 0, 2, 0x65, 0x80];
        assert!(closed_idr(VideoCodec::H264, &avc));
        assert!(!closed_idr(
            VideoCodec::H264,
            &[avc.as_slice(), &[0, 0, 0, 2, 0x41, 0x80]].concat()
        ));
        assert!(!closed_idr(VideoCodec::H264, &[0, 0, 0, 5, 0x65]));
        assert!(closed_idr(
            VideoCodec::Hevc,
            &[0, 0, 0, 3, 19 << 1, 1, 0x80]
        ));
        assert!(!closed_idr(
            VideoCodec::Hevc,
            &[0, 0, 0, 3, 21 << 1, 1, 0x80]
        ));
    }
}
