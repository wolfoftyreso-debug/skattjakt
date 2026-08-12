//! Delivering what the outbox holds.
//!
//! `skattjakt-store::notifications` decides *that* a customer should be told
//! something. This decides *how it reads* and puts it on the wire.
//!
//! ## What a notification may say
//!
//! The single rule this crate exists to enforce:
//!
//! > A notification carries that something happened. It never carries what was
//! > found.
//!
//! A push notification is displayed on a lock screen, which is the one surface
//! the customer does not control — a colleague, a fellow passenger, anyone who
//! picks the phone up. "Din analys är klar" belongs there. "Vi hittade 186 000
//! kr" does not, and neither does a company name, because the two together tell
//! a stranger which business is sitting on an unclaimed deduction.
//!
//! Email is a little less exposed and is treated the same way, because it is
//! forwarded, indexed by mail providers, and read on the same lock screen.
//!
//! The rendering functions below take an identifier and a kind. They are not
//! given an amount, so they cannot leak one — the same structural argument as
//! `LogRecord::message` being `&'static str`.

pub mod email;
pub mod render;
pub mod sender;

pub use email::{SmtpConfig, SmtpSender};
pub use render::{Rendered, RenderedPush};
pub use sender::{
    ChannelSender, DeliveryError, DeliveryOutcome, Dispatcher, PushSender, PushToken, Recipient,
};

#[cfg(test)]
mod notify_tests;
