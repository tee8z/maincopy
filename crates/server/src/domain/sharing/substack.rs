//! Substack offers no publishing API. This client speaks the private web API
//! its own editor uses, signed in with the Owner's session cookie. Substack
//! can change any of it without notice, so an unexpected answer fails closed.

use reqwest::{
    Client, Method, Request, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, COOKIE, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use thiserror::Error;

use super::{
    http::{Exchange, exchange},
    settings::SubstackSettings,
    store::{DeliveryFailure, DeliveryOutcome},
    teaser::{Teaser, TeaserView},
};

const ACCOUNT_ORIGIN: &str = "https://substack.com/";
const MAX_SLUG_BYTES: usize = 200;
const UNEXPECTED: DeliveryOutcome = DeliveryOutcome::Failed(DeliveryFailure::UnexpectedResponse);

pub(super) struct SubstackClient {
    http: Client,
    /// Answers who the session belongs to.
    account: Url,
    /// The publication's own host, which owns its drafts and posts.
    publication: Url,
    session: HeaderValue,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the saved Substack settings cannot form a request")]
pub(super) struct SubstackSetupError;

#[derive(Serialize)]
struct NewDraft<'a> {
    draft_title: &'a str,
    draft_subtitle: &'static str,
    /// Substack stores the editor's document as a JSON string.
    draft_body: String,
    draft_bylines: [Byline; 1],
    audience: &'static str,
    write_comment_permissions: &'static str,
    /// Publications with sections refuse to publish a draft that never chose one.
    section_chosen: bool,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct Byline {
    id: u64,
    is_guest: bool,
}

#[derive(Serialize)]
struct Publish {
    /// `false` publishes to the web only; Substack emails nobody.
    send: bool,
    share_automatically: bool,
}

#[derive(Deserialize)]
struct Profile {
    id: u64,
}

#[derive(Deserialize)]
struct Draft {
    id: u64,
}

#[derive(Deserialize)]
struct Published {
    slug: Option<String>,
}

impl SubstackClient {
    pub(super) fn new(
        http: Client,
        settings: &SubstackSettings,
    ) -> Result<Self, SubstackSetupError> {
        let publication = format!("https://{}.substack.com/", settings.subdomain.as_str());
        let mut session =
            HeaderValue::from_str(&format!("substack.sid={}", settings.session.expose()))
                .map_err(|_| SubstackSetupError)?;
        session.set_sensitive(true);
        Ok(Self {
            http,
            account: Url::parse(ACCOUNT_ORIGIN).map_err(|_| SubstackSetupError)?,
            publication: Url::parse(&publication).map_err(|_| SubstackSetupError)?,
            session,
        })
    }

    /// Create the draft unless an earlier attempt already did, then publish it.
    pub(super) async fn deliver(&self, teaser: &Teaser, draft: Option<u64>) -> DeliveryOutcome {
        let draft = match draft {
            Some(draft) => draft,
            None => match self.create_draft(teaser.view()).await {
                Ok(draft) => draft,
                Err(outcome) => return outcome,
            },
        };
        self.publish(draft).await
    }

    async fn create_draft(&self, teaser: TeaserView<'_>) -> Result<u64, DeliveryOutcome> {
        let profile = self.account.join("api/v1/user/profile/self");
        let author: Profile = self.read(Method::GET, profile, None, None).await?;
        let draft = NewDraft {
            draft_title: teaser.title,
            draft_subtitle: "",
            draft_body: document(teaser),
            draft_bylines: [Byline {
                id: author.id,
                is_guest: false,
            }],
            audience: "everyone",
            write_comment_permissions: "everyone",
            section_chosen: true,
            kind: "newsletter",
        };
        let body = serde_json::to_vec(&draft).map_err(|_| UNEXPECTED)?;
        let drafts = self.publication.join("api/v1/drafts");
        let created: Draft = self.read(Method::POST, drafts, Some(body), None).await?;
        Ok(created.id)
    }

    async fn publish(&self, draft: u64) -> DeliveryOutcome {
        let Ok(body) = serde_json::to_vec(&Publish {
            send: false,
            share_automatically: false,
        }) else {
            return UNEXPECTED;
        };
        let publish = self
            .publication
            .join(&format!("api/v1/drafts/{draft}/publish"));
        let published: Published = match self
            .read(Method::POST, publish, Some(body), Some(draft))
            .await
        {
            Ok(published) => published,
            Err(outcome) => return outcome,
        };
        // The post is public either way; an unreadable slug only loses the deep link.
        let url = published
            .slug
            .filter(|slug| valid_slug(slug))
            .and_then(|slug| self.publication.join(&format!("p/{slug}")).ok())
            .unwrap_or_else(|| self.publication.clone());
        DeliveryOutcome::Posted { url: url.into() }
    }

    /// Republishing a published draft changes nothing, so every unknown or
    /// transient answer is safe to retry with the same draft.
    async fn read<Reply: DeserializeOwned>(
        &self,
        method: Method,
        url: Result<Url, url::ParseError>,
        body: Option<Vec<u8>>,
        draft: Option<u64>,
    ) -> Result<Reply, DeliveryOutcome> {
        let Ok(url) = url else {
            return Err(UNEXPECTED);
        };
        let mut request = Request::new(method, url);
        let headers = request.headers_mut();
        headers.insert(COOKIE, self.session.clone());
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(body) = body {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            *request.body_mut() = Some(body.into());
        }
        let (status, body) = match exchange(&self.http, request).await {
            Exchange::Replied { status, body } => (status, body),
            Exchange::Unreachable | Exchange::Unknown => {
                return Err(DeliveryOutcome::Retry { draft });
            }
        };
        match status {
            StatusCode::OK => serde_json::from_slice(&body).map_err(|_| UNEXPECTED),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(DeliveryOutcome::CredentialsRejected { draft })
            }
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
                Err(DeliveryOutcome::Retry { draft })
            }
            status if status.is_server_error() => Err(DeliveryOutcome::Retry { draft }),
            status if status.is_client_error() => {
                Err(DeliveryOutcome::Failed(DeliveryFailure::Refused))
            }
            _ => Err(UNEXPECTED),
        }
    }
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= MAX_SLUG_BYTES
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The post body in the editor's document format: the summary when present,
/// then the article link. The title is the post's own headline.
fn document(teaser: TeaserView<'_>) -> String {
    let link = json!({
        "type": "paragraph",
        "content": [{
            "type": "text",
            "text": teaser.url,
            "marks": [{"type": "link", "attrs": {"href": teaser.url}}],
        }],
    });
    let content = match teaser.summary.is_empty() {
        true => vec![link],
        false => vec![
            json!({"type": "paragraph", "content": [{"type": "text", "text": teaser.summary}]}),
            link,
        ],
    };
    json!({"type": "doc", "content": content}).to_string()
}

#[cfg(test)]
mod tests;
