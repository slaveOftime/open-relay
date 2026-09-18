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
            },
        );
        (id, ControlOutcome { role, demoted })
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
    #[allow(dead_code)]
    pub fn report_applied(&mut self, id: u64, cursor: u64) {
        if let Some(attachment) = self.attachments.get_mut(&id) {
            attachment.applied_cursor = cursor;
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

    /// All live attachments, for status surfaces.
    /// M3 status surface: enumerate live attachments.
    #[allow(dead_code)]
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
}
