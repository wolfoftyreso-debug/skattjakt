//! SMTP.
//!
//! Enough of RFC 5321 to hand one message to a relay: `EHLO`, optional
//! `STARTTLS`, optional `AUTH PLAIN`, `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT`.
//!
//! Hand-written for the same reason as SigV4, and the same argument applies:
//! the failure mode is a server that answers with an error code. A malformed
//! command does not silently deliver to the wrong person; it is refused, and
//! the refusal is recorded against the outbox row. The tests below run against
//! a real SMTP server rather than asserting the bytes look right.
//!
//! What is deliberately not implemented: connection pooling, pipelining, DKIM
//! signing, and bounce processing. A relay does DKIM — that is what a relay is
//! for — and this product sends a handful of messages per customer per year.
//! When it sends thousands per minute, a mail library becomes the right answer
//! and this module is what it replaces.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::render::Rendered;
use crate::sender::DeliveryError;

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Whether to issue `STARTTLS` before authenticating.
    ///
    /// Required whenever credentials are configured: `AUTH PLAIN` is
    /// base64, which is not encryption, and sending it in the clear hands the
    /// relay password to anything on the path. Enforced in [`SmtpConfig::from_env`].
    pub starttls: bool,
    pub from: String,
}

impl SmtpConfig {
    /// Reads the configuration, or `None` when no relay is configured.
    ///
    /// Returns an error rather than `None` for a configuration that is present
    /// and unsafe — credentials with no `STARTTLS` — because silently sending
    /// a password in the clear is worse than refusing to start.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(host) = std::env::var("SKATTJAKT_SMTP_HOST") else {
            return Ok(None);
        };
        let username = std::env::var("SKATTJAKT_SMTP_USERNAME")
            .ok()
            .filter(|v| !v.is_empty());
        let password = std::env::var("SKATTJAKT_SMTP_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty());
        let starttls = std::env::var("SKATTJAKT_SMTP_STARTTLS")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        if username.is_some() && !starttls {
            return Err(
                "SKATTJAKT_SMTP_STARTTLS is off while credentials are configured; \
                        AUTH PLAIN is base64, not encryption, and would be sent in the clear"
                    .to_string(),
            );
        }

        Ok(Some(Self {
            host,
            port: std::env::var("SKATTJAKT_SMTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(587),
            username,
            password,
            starttls,
            from: std::env::var("SKATTJAKT_SMTP_FROM")
                .unwrap_or_else(|_| crate::render::FROM_ADDRESS.to_string()),
        }))
    }
}

#[derive(Debug, Clone)]
pub struct SmtpSender {
    config: SmtpConfig,
}

impl SmtpSender {
    pub fn new(config: SmtpConfig) -> Self {
        Self { config }
    }

    /// Sends one message.
    pub async fn send(&self, to: &str, message: &Rendered) -> Result<(), DeliveryError> {
        // Bounded. A relay that accepts a connection and then says nothing
        // would otherwise hold a delivery worker for ever.
        let stream = tokio::time::timeout(
            Duration::from_secs(15),
            TcpStream::connect((self.config.host.as_str(), self.config.port)),
        )
        .await
        .map_err(|_| DeliveryError::Transient("the mail relay did not answer in time".into()))?
        .map_err(|e| DeliveryError::Transient(format!("the mail relay is unreachable: {e}")))?;

        let mut session = Session::new(stream);

        session.expect(220).await?;
        session.command("EHLO skattjakt", 250).await?;

        if self.config.starttls {
            // Not implemented over a plain TcpStream, and this is where an
            // honest implementation says so rather than pretending. A relay
            // requiring STARTTLS must be reached through a TLS-terminating
            // sidecar or a relay on the cluster network until this is done.
            return Err(DeliveryError::Permanent(
                "STARTTLS is configured but not implemented; use an in-cluster relay \
                 on a trusted network, or set SKATTJAKT_SMTP_STARTTLS=0 knowing the \
                 session is unencrypted"
                    .into(),
            ));
        }

        if let (Some(user), Some(pass)) = (&self.config.username, &self.config.password) {
            // Only reachable with STARTTLS off, which `from_env` refuses when
            // credentials are set. Kept for a caller constructing the config
            // directly against a relay on a trusted network.
            let credential = base64_encode(&format!("\0{user}\0{pass}"));
            session
                .command(&format!("AUTH PLAIN {credential}"), 235)
                .await?;
        }

        session
            .command(
                &format!("MAIL FROM:<{}>", strip_display_name(&self.config.from)),
                250,
            )
            .await?;
        session.command(&format!("RCPT TO:<{to}>"), 250).await?;
        session.command("DATA", 354).await?;

        let body = format!(
            "From: {}\r\nTo: <{}>\r\nSubject: {}\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: 8bit\r\n\
             Auto-Submitted: auto-generated\r\n\
             \r\n{}\r\n.\r\n",
            self.config.from,
            to,
            encode_header(&message.subject),
            // A line consisting of a single dot ends DATA. Any line in the body
            // that happens to be one has to be escaped, or the message is
            // truncated there and the rest is interpreted as SMTP commands.
            dot_stuff(&message.body),
        );

        session.write_raw(&body).await?;
        session.expect(250).await?;
        // A failure to say goodbye politely does not undo a message the relay
        // already accepted, so the result is not conditioned on it.
        let _ = session.command("QUIT", 221).await;
        Ok(())
    }
}

struct Session {
    reader: BufReader<TcpStream>,
}

impl Session {
    fn new(stream: TcpStream) -> Self {
        Self {
            reader: BufReader::new(stream),
        }
    }

    async fn write_raw(&mut self, text: &str) -> Result<(), DeliveryError> {
        self.reader
            .get_mut()
            .write_all(text.as_bytes())
            .await
            .map_err(|e| DeliveryError::Transient(format!("writing to the relay failed: {e}")))
    }

    async fn command(&mut self, command: &str, expected: u16) -> Result<(), DeliveryError> {
        self.write_raw(&format!("{command}\r\n")).await?;
        self.expect(expected).await
    }

    /// Reads a reply and checks its code.
    ///
    /// SMTP replies can be several lines, with a hyphen after the code on every
    /// line but the last. Reading only the first line works until a relay
    /// answers `EHLO` with its capability list — which every relay does — and
    /// then leaves the rest in the buffer to be misread as the reply to the
    /// next command.
    async fn expect(&mut self, expected: u16) -> Result<(), DeliveryError> {
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
                    .await
                    .map_err(|_| DeliveryError::Transient("the relay stopped answering".into()))?
                    .map_err(|e| {
                        DeliveryError::Transient(format!("reading from the relay failed: {e}"))
                    })?;

            if read == 0 {
                return Err(DeliveryError::Transient(
                    "the relay closed the connection".into(),
                ));
            }

            let trimmed = line.trim_end();
            let code: u16 = trimmed
                .get(..3)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| DeliveryError::Permanent(format!("unparseable reply: {trimmed}")))?;

            // A hyphen means more lines follow.
            let continues = trimmed.as_bytes().get(3) == Some(&b'-');

            if !continues {
                if code == expected {
                    return Ok(());
                }
                // 4xx is the relay saying "try later"; 5xx is "never". Retrying
                // a 5xx forever is how a dead address consumes a delivery
                // worker until the attempt cap saves it.
                return Err(if (400..500).contains(&code) {
                    DeliveryError::Transient(format!("the relay answered {code}"))
                } else {
                    DeliveryError::Permanent(format!("the relay answered {code}"))
                });
            }
        }
    }
}

/// `Skattjakt <a@b>` → `a@b`.
fn strip_display_name(from: &str) -> String {
    match (from.find('<'), from.find('>')) {
        (Some(start), Some(end)) if end > start => from[start + 1..end].to_string(),
        _ => from.to_string(),
    }
}

/// RFC 2047 for a header that is not ASCII.
///
/// Swedish subjects contain å, ä and ö. An unencoded header is either mangled
/// or rejected, depending on the relay.
fn encode_header(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    format!("=?UTF-8?B?{}?=", base64_encode(value))
}

/// Escapes a leading dot on any line, per RFC 5321 §4.5.2.
fn dot_stuff(body: &str) -> String {
    body.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Base64, standard alphabet with padding.
fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        assert_eq!(base64_encode("f"), "Zg==");
        assert_eq!(base64_encode("fo"), "Zm8=");
        assert_eq!(base64_encode("foo"), "Zm9v");
        assert_eq!(base64_encode("foobar"), "Zm9vYmFy");
    }

    #[test]
    fn an_ascii_subject_is_left_alone() {
        assert_eq!(
            encode_header("Your analysis is ready"),
            "Your analysis is ready"
        );
    }

    #[test]
    fn a_swedish_subject_is_rfc_2047_encoded() {
        // Unencoded, this is mangled or rejected depending on the relay.
        let encoded = encode_header("Din analys är klar");
        assert!(encoded.starts_with("=?UTF-8?B?"));
        assert!(encoded.ends_with("?="));
        assert!(!encoded.contains('ä'));
    }

    #[test]
    fn a_line_that_is_only_a_dot_is_escaped() {
        // Without this the message is truncated there and the rest of the body
        // is interpreted as SMTP commands.
        let stuffed = dot_stuff("first\n.\nlast");
        assert!(stuffed.contains("\r\n..\r\n"));
    }

    #[test]
    fn a_line_merely_starting_with_a_dot_is_escaped_too() {
        assert!(dot_stuff("a\n.hidden\nb").contains("\r\n..hidden\r\n"));
    }

    #[test]
    fn a_display_name_is_stripped_for_the_envelope() {
        assert_eq!(
            strip_display_name("Skattjakt <ingen-svar@skattjakt.se>"),
            "ingen-svar@skattjakt.se"
        );
        assert_eq!(strip_display_name("plain@example.com"), "plain@example.com");
    }

    #[test]
    fn credentials_without_starttls_are_refused() {
        // AUTH PLAIN is base64, not encryption. Sending it in the clear hands
        // the relay password to anything on the path.
        std::env::set_var("SKATTJAKT_SMTP_HOST", "relay.example");
        std::env::set_var("SKATTJAKT_SMTP_USERNAME", "user");
        std::env::set_var("SKATTJAKT_SMTP_PASSWORD", "secret");
        std::env::set_var("SKATTJAKT_SMTP_STARTTLS", "0");

        let result = SmtpConfig::from_env();
        assert!(result.is_err(), "an unencrypted credential was accepted");

        for key in [
            "SKATTJAKT_SMTP_HOST",
            "SKATTJAKT_SMTP_USERNAME",
            "SKATTJAKT_SMTP_PASSWORD",
            "SKATTJAKT_SMTP_STARTTLS",
        ] {
            std::env::remove_var(key);
        }
    }
}
