//! Email through the farm's own SMTP server (lettre, rustls on ring): plain
//! text, STARTTLS (587), TLS (465) or, for a mail server on the farm's own
//! network, no encryption.

use std::time::Duration;

use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use op_core::Ctx;
use op_core::alert::MessageLog;

use super::{Channel, ChannelError, ChannelsConfig, Delivery, Tls, secret, set};

/// Subject when a message has none.
pub const DEFAULT_SUBJECT: &str = "openpasture";

#[derive(Clone)]
pub struct Email {
    host: String,
    port: u16,
    tls: Tls,
    user: Option<String>,
    password: Option<String>,
    from: Mailbox,
}

impl Email {
    /// The farm's SMTP settings, or `None` when the host or from address is
    /// missing (or a user is set without a password).
    pub fn from_config(ctx: &Ctx, cfg: &ChannelsConfig) -> anyhow::Result<Option<Self>> {
        let e = &cfg.email;
        let (Some(host), Some(from)) = (set(&e.host), set(&e.from)) else { return Ok(None) };
        let user = set(&e.user).map(str::to_owned);
        let password = secret(ctx, "smtp_password")?;
        if user.is_some() && password.is_none() {
            return Ok(None);
        }
        let Ok(address) = from.parse() else { return Ok(None) };
        Ok(Some(Self { host: host.to_owned(), port: e.port, tls: e.tls, user, password, from: Mailbox::new(Some("openpasture".into()), address) }))
    }

    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, ChannelError> {
        let builder = match self.tls {
            Tls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host),
            Tls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host),
            Tls::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host)),
        }
        .map_err(|e| ChannelError::Fail(format!("SMTP: {e}")))?;
        let mut builder = builder.port(self.port).timeout(Some(Duration::from_secs(20)));
        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }
        Ok(builder.build())
    }
}

/// Server said 4xx, or the connection dropped: try again. 5xx, TLS and
/// login problems: fix the settings.
fn smtp_error(e: &lettre::transport::smtp::Error) -> ChannelError {
    let text = format!("SMTP: {e}");
    if e.is_permanent() || e.is_tls() || e.is_client() || e.is_response() { ChannelError::Fail(text) } else { ChannelError::Retry(text) }
}

#[async_trait::async_trait]
impl Channel for Email {
    fn kind(&self) -> &'static str {
        "email"
    }

    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError> {
        let to: Mailbox = msg.address.trim().parse().map_err(|_| ChannelError::Fail("That email address doesn't look right.".into()))?;
        let subject = msg.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SUBJECT);
        let email = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(msg.text.clone())
            .map_err(|e| ChannelError::Fail(format!("Email: {e}")))?;
        self.transport()?.send(email).await.map_err(|e| smtp_error(&e))?;
        // SMTP queue ids repeat over time, so none is kept (provider ids are unique per channel).
        Ok(Delivery::sent(None))
    }
}
