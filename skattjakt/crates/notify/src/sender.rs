//! Dispatch: one notification, several channels, per-channel outcomes.

use async_trait::async_trait;
use skattjakt_store::notifications::{Channel, NotificationKind, PendingNotification};
pub use skattjakt_store::notifications::{PushToken, Recipient};
use skattjakt_telemetry::LogRecord;

/// Why a delivery failed.
///
/// The distinction decides whether the outbox row is retried. Collapsing them
/// would either retry a dead address until the attempt cap saves it, or give up
/// on a relay that was restarting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// Try again later — a 4xx from a relay, a refused connection, a timeout.
    #[error("{0}")]
    Transient(String),
    /// Do not try again — a 5xx, an unknown recipient, a dead push token.
    #[error("{0}")]
    Permanent(String),
    /// There is no sender for this channel in this deployment.
    #[error("the {0} channel is not configured")]
    NotConfigured(&'static str),
}

/// What one attempt achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryOutcome {
    pub delivered: Vec<String>,
    /// `None` when everything asked for was delivered.
    pub error: Option<String>,
    /// Whether the failure is worth another attempt.
    pub retry: bool,
}

/// One channel's transport.
#[async_trait]
pub trait ChannelSender: Send + Sync {
    async fn send(
        &self,
        kind: NotificationKind,
        recipient: &Recipient,
        subject_id: Option<uuid::Uuid>,
    ) -> Result<(), DeliveryError>;
}

/// APNs and FCM.
///
/// Deliberately unimplemented, and this is the honest form of that (§31): a
/// type that exists, is wired in, and answers `NotConfigured` — rather than a
/// stub that logs "sent!" and returns success, which would make the outbox
/// report deliveries that never happened.
///
/// What is already done, so that implementing this is implementing a transport
/// and nothing else: device registration, per-provider tokens, dead-token
/// marking, per-kind channel defaults, the retry schedule, and the rendering.
#[derive(Debug, Default)]
pub struct PushSender;

#[async_trait]
impl ChannelSender for PushSender {
    async fn send(
        &self,
        _kind: NotificationKind,
        _recipient: &Recipient,
        _subject_id: Option<uuid::Uuid>,
    ) -> Result<(), DeliveryError> {
        // Permanent rather than transient: no amount of retrying will configure
        // a provider, and retrying would fill the queue with attempts that
        // cannot succeed.
        Err(DeliveryError::NotConfigured("push"))
    }
}

/// The in-app channel.
///
/// Delivery is the row existing: the client reads notifications from the API,
/// so there is nothing to transmit. It is still a channel rather than an
/// implicit default, because a customer who has turned in-app off should not
/// see it, and "delivered" should mean the same thing everywhere.
#[derive(Debug, Default)]
pub struct InAppSender;

#[async_trait]
impl ChannelSender for InAppSender {
    async fn send(
        &self,
        _kind: NotificationKind,
        _recipient: &Recipient,
        _subject_id: Option<uuid::Uuid>,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }
}

/// Email, over SMTP.
#[async_trait]
impl ChannelSender for crate::email::SmtpSender {
    async fn send(
        &self,
        kind: NotificationKind,
        recipient: &Recipient,
        subject_id: Option<uuid::Uuid>,
    ) -> Result<(), DeliveryError> {
        let Some(address) = recipient.email.as_deref() else {
            // No address is permanent: retrying will not produce one.
            return Err(DeliveryError::Permanent(
                "the recipient has no email address".into(),
            ));
        };
        let message = crate::render::email(kind, subject_id);
        crate::email::SmtpSender::send(self, address, &message).await
    }
}

/// Routes one notification to the channels it asked for.
pub struct Dispatcher {
    email: Option<Box<dyn ChannelSender>>,
    push: Box<dyn ChannelSender>,
    in_app: Box<dyn ChannelSender>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("email_configured", &self.email.is_some())
            .finish()
    }
}

impl Dispatcher {
    pub fn new(email: Option<Box<dyn ChannelSender>>) -> Self {
        Self {
            email,
            push: Box::new(PushSender),
            in_app: Box::new(InAppSender),
        }
    }

    /// Delivers one notification, returning what actually went out.
    ///
    /// Each channel is attempted independently and the successes are reported
    /// even when another channel fails. That is what lets a retry send only
    /// what is missing rather than sending the email a second time.
    pub async fn deliver(
        &self,
        notification: &PendingNotification,
        recipient: &Recipient,
    ) -> DeliveryOutcome {
        let Some(kind) = parse_kind(&notification.kind) else {
            // A kind nobody has written words for would be delivered as a blank
            // notification, which is worse than not delivering it.
            return DeliveryOutcome {
                delivered: Vec::new(),
                error: Some(format!("unknown notification kind: {}", notification.kind)),
                retry: false,
            };
        };

        let mut delivered = Vec::new();
        let mut errors = Vec::new();
        let mut retry = false;

        for channel_name in &notification.channels {
            let Some(channel) = Channel::parse(channel_name) else {
                continue;
            };
            let sender = match channel {
                Channel::Email => self.email.as_deref(),
                Channel::Push => Some(self.push.as_ref()),
                Channel::InApp => Some(self.in_app.as_ref()),
            };

            let result = match sender {
                Some(sender) => sender.send(kind, recipient, notification.subject_id).await,
                None => Err(DeliveryError::NotConfigured("email")),
            };

            match result {
                Ok(()) => delivered.push(channel.as_str().to_string()),
                Err(error) => {
                    if matches!(error, DeliveryError::Transient(_)) {
                        retry = true;
                    }
                    // The channel and the failure class, never the address and
                    // never the relay's message — a bounce message quotes the
                    // recipient, and a recipient names a person.
                    LogRecord::warn("a notification channel failed")
                        .public("channel", channel.as_str())
                        .public("class", classify(&error))
                        .internal("correlation_id", notification.correlation_id.to_string())
                        .emit();
                    errors.push(format!("{}: {}", channel.as_str(), classify(&error)));
                }
            }
        }

        DeliveryOutcome {
            delivered,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
            retry,
        }
    }
}

/// The failure *class*, for `notifications.last_error`.
///
/// Never the relay's own message: a bounce quotes the recipient address, and an
/// address in an operator's queue view is a person's identity in a table many
/// people can read.
fn classify(error: &DeliveryError) -> &'static str {
    match error {
        DeliveryError::Transient(_) => "transient",
        DeliveryError::Permanent(_) => "permanent",
        DeliveryError::NotConfigured(_) => "not_configured",
    }
}

fn parse_kind(value: &str) -> Option<NotificationKind> {
    match value {
        "analysis_completed" => Some(NotificationKind::AnalysisCompleted),
        "analysis_failed" => Some(NotificationKind::AnalysisFailed),
        "document_processed" => Some(NotificationKind::DocumentProcessed),
        "member_invited" => Some(NotificationKind::MemberInvited),
        "security_alert" => Some(NotificationKind::SecurityAlert),
        _ => None,
    }
}
