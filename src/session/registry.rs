//! Attachment registry (fenced attachments with role + liveness).
//!
//! Replaces anonymous attach counters with identified attachment records:
//! every attached client gets a per-session attachment id (a fencing token:
//! stale or unknown ids cannot act on the session), a role, and liveness.
//!
//! Policy: **observe is the only restricted role**. Every attach is a
//! `Controller` by default — it may send input and resize the PTY, and
//! several controllers may drive at once (the last successful resize wins
//! on geometry). A CLI attach that explicitly asks for view-only mode
//! (`oly attach --observe`) registers as an `Observer`: it watches, reports
//! its viewport, but input and resize are rejected with a clear error.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What kind of client an attachment belongs to (principal class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKind {
    /// Interactive CLI (`oly attach` over IPC).
    Cli,
    /// Browser client over WebSocket.
    Web,
}

/// The role an attachment holds for its whole lifetime. It is chosen at
/// register time and never changes: there is no lease, no takeover, and no
/// demotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachRole {
    /// View-only (`oly attach --observe`): input and resize are rejected.
    Observer,
    /// Full control: input and geometry. The default for every attach.
    Controller,
}

impl AttachRole {
    /// Parse the wire/CLI role token (`None` defaults to `Controller`).
    pub fn parse(role: Option<&str>) -> Result<Self, String> {
        match role {
            None | Some("controller") | Some("control") => Ok(Self::Controller),
            Some("observe") | Some("observer") => Ok(Self::Observer),
            Some(other) => Err(format!(
                "invalid attach mode {other:?}: expected observe|controller"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }
}

/// One identified attachment. Some fields feed status-list surfaces; the
/// binary crate flags the rest as `#[allow(dead_code)]` until those
/// surfaces land.
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
    /// Last stream cursor the client reported as fully applied. Shared
    /// with the attachment's output pump, which gates sends on it
    /// (credits are enforced, not advisory, so a slow client cannot
    /// unbounded-buffer the daemon).
    pub applied_cursor: Arc<AtomicU64>,
}

/// Per-session registry. Lives inside the session runtime so every
/// transition is sequenced under the same write lock that sequences output.
#[derive(Debug, Default)]
pub struct AttachmentRegistry {
    next_id: u64,
    attachments: HashMap<u64, Attachment>,
}

impl AttachmentRegistry {
    /// Register a new attachment with the role chosen at attach time.
    pub fn register(
        &mut self,
        kind: AttachKind,
        role: AttachRole,
        viewport: Option<(u16, u16)>,
    ) -> (u64, AttachRole) {
        self.next_id += 1;
        let id = self.next_id;
        self.attachments.insert(
            id,
            Attachment {
                id,
                kind,
                role,
                connected_at: Instant::now(),
                viewport,
                applied_cursor: Arc::new(AtomicU64::new(0)),
            },
        );
        (id, role)
    }

    /// Remove an attachment. Returns the removed record; unknown ids
    /// (stale fencing tokens) are a no-op.
    pub fn unregister(&mut self, id: u64) -> Option<Attachment> {
        self.attachments.remove(&id)
    }

    /// May this attachment drive input/geometry? Controllers can;
    /// observers (view-only attaches) cannot; stale tokens cannot.
    pub fn can_control(&self, id: u64) -> bool {
        self.attachments
            .get(&id)
            .is_some_and(|a| a.role == AttachRole::Controller)
    }

    /// Is this id a live attachment (fencing check)?
    pub fn contains(&self, id: u64) -> bool {
        self.attachments.contains_key(&id)
    }

    /// Record a client-reported applied cursor (drives the pump's credit
    /// gate).
    pub fn report_applied(&mut self, id: u64, cursor: u64) {
        if let Some(attachment) = self.attachments.get_mut(&id) {
            // Credits are monotonic: a stale or duplicated report never
            // moves the cursor backwards (a stale report never regresses
            // the headroom the credit-gate relies on).
            attachment
                .applied_cursor
                .fetch_max(cursor, Ordering::Relaxed);
        }
    }

    /// The shared applied-cursor cell for one attachment, so its output
    /// pump can gate sends on applied + budget (apply-vs-budget headroom
    /// is the contract the credit-gate enforces).
    pub fn credit_cell(&self, id: u64) -> Option<Arc<AtomicU64>> {
        self.attachments
            .get(&id)
            .map(|a| Arc::clone(&a.applied_cursor))
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

    /// Status surface: enumerate live attachments.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn attachments(&self) -> impl Iterator<Item = &Attachment> {
        self.attachments.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applied-cursor reports land in the attachment's shared cell
    /// (which the output pump gates on) and never move it backwards.
    #[test]
    fn report_applied_advances_the_shared_credit_cell_monotonically() {
        let mut registry = AttachmentRegistry::default();
        let (id, _) = registry.register(AttachKind::Cli, AttachRole::Controller, None);
        let cell = registry.credit_cell(id).expect("credit cell");
        assert_eq!(cell.load(Ordering::Relaxed), 0);

        registry.report_applied(id, 4096);
        assert_eq!(cell.load(Ordering::Relaxed), 4096);
        // Stale/duplicate reports never move the cursor backwards.
        registry.report_applied(id, 1024);
        assert_eq!(cell.load(Ordering::Relaxed), 4096);
        // Unknown tokens are ignored.
        registry.report_applied(9999, 1 << 40);
        assert_eq!(cell.load(Ordering::Relaxed), 4096);
        assert!(registry.credit_cell(9999).is_none());
    }

    #[test]
    fn every_controller_request_is_granted_observers_are_gated() {
        let mut registry = AttachmentRegistry::default();
        let (a, role) = registry.register(AttachKind::Cli, AttachRole::Controller, None);
        assert_eq!(role, AttachRole::Controller);
        // A second controller is granted control too — no lease to fight over.
        let (b, role) = registry.register(AttachKind::Web, AttachRole::Controller, None);
        assert_eq!(role, AttachRole::Controller);
        assert!(registry.can_control(a));
        assert!(registry.can_control(b));

        // An explicit observe attach is view-only for its whole lifetime.
        let (c, role) = registry.register(AttachKind::Cli, AttachRole::Observer, None);
        assert_eq!(role, AttachRole::Observer);
        assert!(!registry.can_control(c));
        // ...and it changes nothing for the existing controllers.
        assert!(registry.can_control(a));
        assert!(registry.can_control(b));
    }

    #[test]
    fn unregister_leaves_the_other_attachments_alone() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, AttachRole::Controller, None);
        let (b, _) = registry.register(AttachKind::Web, AttachRole::Observer, None);
        registry.unregister(b);
        assert!(registry.can_control(a));
        assert!(!registry.can_control(b));
        registry.unregister(a);
        assert!(registry.is_empty());
    }

    #[test]
    fn stale_ids_cannot_act() {
        let mut registry = AttachmentRegistry::default();
        let (a, _) = registry.register(AttachKind::Cli, AttachRole::Controller, None);
        registry.unregister(a);
        assert!(!registry.contains(a));
        assert!(!registry.can_control(a));
        assert!(registry.unregister(a).is_none());
    }

    #[test]
    fn roles_parse_the_wire_tokens() {
        assert_eq!(AttachRole::parse(None), Ok(AttachRole::Controller));
        assert_eq!(
            AttachRole::parse(Some("controller")),
            Ok(AttachRole::Controller)
        );
        assert_eq!(AttachRole::parse(Some("observe")), Ok(AttachRole::Observer));
        assert_eq!(
            AttachRole::parse(Some("observer")),
            Ok(AttachRole::Observer)
        );
        assert!(AttachRole::parse(Some("takeover")).is_err());
    }
}
