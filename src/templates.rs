//! HTML templates rendered with maud (compile-time, type-safe). Pages
//! are intentionally minimal - they exist as the visitor's "you did it"
//! confirmation, not as a full website. Brand styling stays at the
//! consumer site; epistole's pages link back home.

use maud::{DOCTYPE, Markup, PreEscaped, html};
use time::OffsetDateTime;

use crate::store::Send;

/// Shared shell - head + body wrapper. `inner` is the page-specific body.
fn shell(brand: &str, title: &str, inner: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (title) " - " (brand) }
                style {
                    "body{font-family:system-ui,sans-serif;max-width:38rem;margin:4rem auto;padding:0 1rem;color:#1c1917;background:#f7f3e8;line-height:1.6}"
                    "h1{font-weight:500;font-size:1.5rem;margin-bottom:1rem}"
                    "p{margin:1rem 0}"
                    "ol{padding-left:1.4rem}"
                    "li{margin:0.75rem 0}"
                    "time{display:block;color:#57534e;font-size:0.9rem}"
                    ".send-body{margin-top:2rem}"
                    ".send-body>*:first-child{margin-top:0}"
                    "a{color:#581523}"
                }
            }
            body {
                main { (inner) }
            }
        }
    }
}

/// Page shown after `POST /subscribe` succeeds - visitor must click the
/// confirmation link in their inbox to activate.
pub(crate) fn pending(brand: &str, email: &str) -> Markup {
    let body = html! {
        h1 { "Almost there." }
        p { "A confirmation link is on its way to " strong { (email) } "." }
        p { "Click the link in that email to finish subscribing. The link expires in 24 hours." }
        p { "If nothing arrives in a few minutes, check your spam folder." }
    };
    shell(brand, "Confirm your subscription", &body)
}

/// Page shown after `GET /confirm?token=...` succeeds.
pub(crate) fn confirmed(brand: &str) -> Markup {
    let body = html! {
        h1 { "Subscribed." }
        p { "You'll hear from " (brand) " when there's something worth saying." }
        p { "Each note carries a one-click unsubscribe link at the bottom." }
    };
    shell(brand, "Subscription confirmed", &body)
}

/// Page shown after `GET /unsubscribe?token=...` succeeds.
pub(crate) fn unsubscribed(brand: &str) -> Markup {
    let body = html! {
        h1 { "Unsubscribed." }
        p { "You'll no longer receive newsletters from " (brand) "." }
        p { "If this was a mistake, you can resubscribe from the contact page on the site." }
    };
    shell(brand, "Unsubscribed", &body)
}

/// Generic error page when a token is invalid / expired / tampered.
pub(crate) fn invalid_token(brand: &str) -> Markup {
    let body = html! {
        h1 { "This link has expired." }
        p { "Confirmation and unsubscribe links are time-limited. Try requesting a fresh one from the contact page." }
    };
    shell(brand, "Link expired", &body)
}

/// Archive index page listing persisted newsletter sends.
pub(crate) fn archive_index(brand: &str, sends: &[Send], truncated: bool) -> Markup {
    let body = html! {
        h1 { "Archive" }
        @if sends.is_empty() {
            p { "Past notes from " (brand) " will appear here once the first issue is sent." }
        } @else {
            ol {
                @for send in sends {
                    li {
                        a href=(format!("/archive/{}", send.id)) { (send.subject) }
                        time datetime=(datetime_attr(send.sent_at)) { (date_label(send.sent_at)) }
                    }
                }
            }
            // WHY: the index is capped, so say so rather than letting a
            // reader believe the list is the complete history.
            @if truncated {
                p { "Showing the most recent " (sends.len()) " notes." }
            }
        }
    };
    shell(brand, "Archive", &body)
}

/// Archive detail page for one immutable send.
pub(crate) fn archive_detail(brand: &str, send: &Send) -> Markup {
    let body_html = ammonia::clean(&send.body_html);
    let body = html! {
        p { a href="/archive" { "Archive" } }
        article {
            h1 { (send.subject) }
            time datetime=(datetime_attr(send.sent_at)) { (date_label(send.sent_at)) }
            div class="send-body" {
                (PreEscaped(body_html))
            }
        }
    };
    shell(brand, &send.subject, &body)
}

fn date_label(sent_at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        sent_at.year(),
        u8::from(sent_at.month()),
        sent_at.day(),
        sent_at.hour(),
        sent_at.minute()
    )
}

fn datetime_attr(sent_at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        sent_at.year(),
        u8::from(sent_at.month()),
        sent_at.day(),
        sent_at.hour(),
        sent_at.minute(),
        sent_at.second()
    )
}
