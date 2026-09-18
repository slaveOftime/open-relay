//! Attachment registry and controller lease (M3-4; PLAN §8.1, invariant I6).
//!
//! Replaces anonymous attach counters with identified attachment records:
//! every attached client gets a per-session attachment id (a fencing token:
//! stale or unknown ids cannot act on the session), a role, and liveness.
//!
//! Policy: **many observers, one controller**. A controller request is
//! granted when the lease is free; otherwise the client joins as an observer
//! and must explicitly take over. Only the controller may drive geometry
//! (resize) and attached input; observers watch. Control changes are
//! published so every client can see who drives (I6 visibility).

use std::collections::HashMap;
use std::time::Instant;

/// What kind of client an attachment belongs to (principal class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// Interactive CLI (`oly attach` over IPC).
    Cli,
    /// Browser client over WebSocket.
    Web,
}

/// Control role requested at attach time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRequest {
    /// Watch only; never takes the lease implicitly.
    Observer,
    /// Drive if the lease is free; otherwise join as observer.
    Controller,
    /// Take the lease even if another attachment holds it (the previous
    /// controller is demoted to observer and can see the handoff).
    Takeover,
}

impl ControlRequest {
    /// Parse the wire/CLI role token (`None` defaults to `Controller`).
    pub fn parse(role: Option<&str>) -> Result<Self, String> {
        match role {
            None => Ok(Self::Controller),
            Some("controller") => Ok(Self::Controller),
            Some("observer") => Ok(Self::Observer),
            Some("takeover") => Ok(Self::Takeover),
            Some(other) => Err(format!(
                "invalid control role {other:?}: expected observer|controller|takeover"
            )),
        }
    }
}

/// The role an attachment actually holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    Observer,
    Controller,
}

impl AttachRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }
}

/// TTL for parked agent control leases (post-review corrective
/// increment): a parked lease belongs to an agent that may crash without
/// ever releasing it, so it expires instead of gating the session — or
/// the attachment map — forever. Gated sends renew it (activity proves
/// liveness). Principal-bound revocation arrives with auth scopes in M5.
pub const PARKED_LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Bound on simultaneously parked leases per session (I7: no unbounded
/// growth, even within the TTL).
pub const MAX_PARKED_LEASES: usize = 8;

/// One identified attachment. Some fields are consumed by later M3 surfaces
/// (applied-cursor ACKs in M3-5, attached-client status listings), hence the
/// allow.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Attachment {
    /// Per-session fencing token, monotonically increasing.
    pub id: u64,
    pub kind: AttachKind,
    pub role: AttachRole,
    pub connected_at: Instant,
    /// Last geometry this attachment declared (its terminal size).
    pub viewport: Option<(u16, u16)>,
    /// Last stream cursor the client reported as fully applied (feeds
    /// credits/backpressure; informational until M3-5).
    pub applied_cursor: u64,
    /// Expiry for parked agent leases (`None` for streaming attachments,
    /// whose lifetime is their connection).
    pub lease_expires_at: Option<Instant>,
}

/// Per-session registry. Lives inside the session runtime so every
/// transition is sequenced under the same write lock that sequences output.
#[derive(Debug, Default)]
pub struct AttachmentRegistry {
    next_id: u64,
    attachments: HashMap<u64, Attachment>,
    controller: Option<u64>,
}

/// Outcome of a registration or control operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlOutcome {
    pub role: AttachRole,
    /// The previous controller's attachment id when a takeover demoted it.
    pub demoted: Option<u64>,
}

impl AttachmentRegistry {
    /// Register a new attachment, granting control per policy.
    pub fn register(
        &mut self,
        kind: AttachKind,
        request: ControlRequest,
        viewport: Option<(u16, u16)>,
    ) -> (u64, ControlOutcome) {
        self.next_id += 1;
        let id = self.next_id;
        let (role, demoted) = match request {
            ControlRequest::Observer => (AttachRole::Observer, None),
            ControlRequest::Controller => match self.controller {
                None => {
                    self.controller = Some(id);
                    (AttachRole::Controller, None)
                }
                // Lease held: join visibly as observer (PLAN §8.1).
                Some(_) => (AttachRole::Observer, None),
            },
            ControlRequest::Takeover => {
                let demoted = self.controller.replace(id);
                (AttachRole::Controller, demoted)
            }
        };
        self.attachments.insert(
            id,
            Attachment {
                id,
                kind,
                role,
                connected_at: Instant::now(),
                viewport,
                applied_cursor: 0,
                lease_expires_at: None,
            },
        );
        (id, ControlOutcome { role, demoted })
    }

    /// Register a parked agent control lease: takeover semantics plus a
    /// TTL. `None` when the parked-lease cap is reached — callers purge
    /// expired leases first, so a refusal means genuinely concurrent
    /// agents, not accumulated corpses.
    pub fn register_parked(
        &mut self,
        kind: AttachKind,
        ttl: std::time::Duration,
        now: Instant,
    ) -> Option<(u64, ControlOutcome)> {
        let parked = self
            .attachments
            .values()
            .filter(|attachment| attachment.lease_expires_at.is_some())
            .count();
        if parked >= MAX_PARKED_LEASES {
            return None;
        }
        let (id, outcome) = self.register(kind, ControlRequest::Takeover, None);
        self.attachments
            .get_mut(&id)
            .expect("just registered")
            .lease_expires_at = Some(now + ttl);
        Some((id, outcome))
    }

    /// Remove every parked lease whose TTL has elapsed, releasing the
    /// control lease if an expired attachment held it. Returns the removed
    /// attachment ids. Streaming attachments (no expiry) are never purged.
    pub fn purge_expired(&mut self, now: Instant) -> Vec<u64> {
        let expired: Vec<u64> = self
            .attachments
            .values()
            .filter(|attachment| {
                attachment
                    .lease_expires_at
                    .is_some_and(|expires_at| expires_at <= now)
            })
            .map(|attachment| attachment.id)
            .collect();
        for id in &expired {
            self.unregister(*id);
        }
        expired
    }

    /// Extend a parked lease's TTL on activity (a gated send proves the
    /// agent is alive). No-op for streaming attachments and unknown ids.
    pub fn renew_lease(&mut self, id: u64, now: Instant) {
        if let Some(attachment) = self.attachments.get_mut(&id)
            && attachment.lease_expires_at.is_some()
        {
            attachment.lease_expires_at = Some(now + PARKED_LEASE_TTL);
        }
    }

    /// Remove an attachment, releasing the lease if it held it. Returns the
    /// removed record; unknown ids (stale fencing tokens) are a no-op.
    pub fn unregister(&mut self, id: u64) -> Option<Attachment> {
        let removed = self.attachments.remove(&id)?;
        if self.controller == Some(id) {
            self.controller = None;
        }
        Some(removed)
    }

    /// Explicitly acquire the lease (takeover by an attached observer).
    pub fn acquire_control(&mut self, id: u64) -> Option<ControlOutcome> {
        let attachment = self.attachments.get_mut(&id)?;
        let demoted = self.controller.replace(id).filter(|old| *old != id);
        attachment.role = AttachRole::Controller;
        if let Some(old) = demoted
            && let Some(previous) = self.attachments.get_mut(&old)
        {
            previous.role = AttachRole::Observer;
        }
        Some(ControlOutcome {
            role: AttachRole::Controller,
            demoted,
        })
    }

    /// Is this attachment the current controller?
    pub fn is_controller(&self, id: u64) -> bool {
        self.controller == Some(id)
    }

    /// The current controller's attachment id.
    pub fn controller_id(&self) -> Option<u64> {
        self.controller
    }

    /// Is this id a live attachment (fencing check)?
    pub fn contains(&self, id: u64) -> bool {
        self.attachments.contains_key(&id)
    }

    /// Record a client-reported applied cursor (drives credits in M3-5).
    pub fn report_applied(&mut self, id: u64, cursor: u64) {
        if let Some(attachment) = self.attachments.get_mut(&id) {
            // Credits are monotonic: a stale or duplicated report never
            // moves the cursor backwards (I7 bookkeeping stays conservative).
            attachment.applied_cursor = attachment.applied_cursor.max(cursor);
        }
    }

    /// Record an attachment's current viewport.
    pub fn set_viewport(&mut self, id: u64, rows: u16, cols: u16) {
        if let Some(attachment) = self.attachments.get_mut(&id) {
            attachment.viewport = Some((rows, cols));
        }
    }

    /// Number of live attachments (replaces the anonymous attach counter).
    pub fn len(&self) -> usize {
        self.attachments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }

    /// M3 status surface: enumerate live attachments.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn attachments(&self) -> impl Iterator<Item = &Attachment> {
        self.attachments.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_controller_request_wins_second_joins_as_observer() {
        let mut registry = AttachmentRegistry::default();
        let (a, outcome) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        assert_eq!(outcome.role, AttachRole::Controller);
        assert_eq!(outcome.demoted, None);

        let (b, outcome) = registry.register(AttachKind::Web, ControlRequest::Controller, None);
        assert_eq!(outcome.role, AttachRole::Observer);
        assert!(registry.is_controller(a));
        assert!(!registry.is_controller(b));
    }

    #[test]
    fn takeover_demotes_the_previous_controller() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        let (b, outcome) = registry.register(AttachKind::Web, ControlRequest::Takeover, None);
        assert_eq!(outcome.role, AttachRole::Controller);
        assert_eq!(outcome.demoted, Some(a));
        assert!(registry.is_controller(b));
        assert!(!registry.is_controller(a));
        // The demoted controller stays attached as an observer.
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn observer_unregister_does_not_release_the_lease() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        let (b, _) = registry.register(AttachKind::Web, ControlRequest::Observer, None);
        registry.unregister(b);
        assert!(registry.is_controller(a));
        registry.unregister(a);
        assert_eq!(registry.controller_id(), None);
        // The lease is free again for the next attachment.
        let (c, outcome) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        assert_eq!(outcome.role, AttachRole::Controller);
        assert!(registry.is_controller(c));
    }

    #[test]
    fn stale_ids_cannot_act() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        registry.unregister(a);
        assert!(!registry.contains(a));
        assert!(registry.acquire_control(a).is_none());
        assert!(registry.unregister(a).is_none());
        assert!(!registry.is_controller(a));
    }

    #[test]
    fn observer_can_take_over_explicitly() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        let (b, _) = registry.register(AttachKind::Web, ControlRequest::Observer, None);
        let outcome = registry.acquire_control(b).expect("attached observer");
        assert_eq!(outcome.demoted, Some(a));
        assert!(registry.is_controller(b));
        // Re-acquiring while holding the lease demotes nobody.
        let outcome = registry.acquire_control(b).expect("still attached");
        assert_eq!(outcome.demoted, None);
    }

    // ------------------------------------------------------------------
    // Parked agent lease lifecycle (post-review corrective increment)
    // ------------------------------------------------------------------

    #[test]
    fn parked_lease_expires_and_frees_the_controller_lease() {
        use std::time::Duration;
        let mut registry = AttachmentRegistry::default();
        let t0 = Instant::now();
        let (stream, _) = registry.register(AttachKind::Cli, ControlRequest::Controller, None);
        let (parked, outcome) = registry
            .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0)
            .expect("parked lease");
        assert_eq!(outcome.role, AttachRole::Controller);
        assert_eq!(outcome.demoted, Some(stream));

        // Alive just before the TTL, gone at it.
        assert!(
            registry
                .purge_expired(t0 + PARKED_LEASE_TTL - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(registry.purge_expired(t0 + PARKED_LEASE_TTL), vec![parked]);
        assert!(!registry.contains(parked));
        assert_eq!(registry.controller_id(), None);

        // The demoted streaming attachment survives and can retake the
        // now-free lease.
        assert!(registry.contains(stream));
        let outcome = registry
            .acquire_control(stream)
            .expect("takeover after expiry");
        assert_eq!(outcome.role, AttachRole::Controller);
    }

    #[test]
    fn parked_lease_renewal_extends_expiry() {
        use std::time::Duration;
        let mut registry = AttachmentRegistry::default();
        let t0 = Instant::now();
        let (parked, _) = registry
            .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0)
            .expect("parked lease");
        // Activity one second before expiry renews the lease from then.
        let renew_at = t0 + PARKED_LEASE_TTL - Duration::from_secs(1);
        registry.renew_lease(parked, renew_at);
        assert!(registry.purge_expired(t0 + PARKED_LEASE_TTL).is_empty());
        assert_eq!(
            registry.purge_expired(renew_at + PARKED_LEASE_TTL),
            vec![parked]
        );
    }

    #[test]
    fn parked_lease_cap_refuses_growth_until_purged() {
        let mut registry = AttachmentRegistry::default();
        let t0 = Instant::now();
        for _ in 0..MAX_PARKED_LEASES {
            registry
                .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0)
                .expect("under the cap");
        }
        assert!(
            registry
                .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0)
                .is_none(),
            "the cap refuses unbounded parked-lease growth"
        );
        // Purging the corpses frees capacity.
        registry.purge_expired(t0 + PARKED_LEASE_TTL);
        assert!(
            registry
                .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0 + PARKED_LEASE_TTL)
                .is_some()
        );
    }

    #[test]
    fn streaming_attachments_are_never_purged() {
        let mut registry = AttachmentRegistry::default();
        let t0 = Instant::now();
        let (stream, _) = registry.register(AttachKind::Web, ControlRequest::Observer, None);
        let (parked, _) = registry
            .register_parked(AttachKind::Cli, PARKED_LEASE_TTL, t0)
            .expect("parked lease");
        let removed = registry.purge_expired(t0 + 10 * PARKED_LEASE_TTL);
        assert_eq!(removed, vec![parked]);
        assert!(registry.contains(stream));
    }
}
