use std::{fmt, str::FromStr};

use blake3::Hasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset};

use super::{
    AssetRevisionReference, CodeRenderingMode, DefaultPostTipPolicy, DigestedAsset,
    DistributionMode, DistributionSettings, DraftStatus, ExternalAssetOrigin, MarkdownDialect,
    MermaidRenderingMode, PostDocument, PostId, PostTipPolicy, PublicationSettings,
    PublicationTipSettings, RawHtmlPolicy, RendererSettings, ResolvedPostAssets,
    ResolvedSiteAssets, SubscriptionSettings,
};

const ASSET_CONTEXT: &str = "maincopy asset digest v1";
const POST_CONTENT_CONTEXT: &str = "maincopy post content digest v1";
const PUBLICATION_CONTENT_CONTEXT: &str = "maincopy publication content digest v1";
const POST_ASSET_SOURCE_BINDING_CONTEXT: &str = "maincopy post asset source binding v1";
const PUBLICATION_ASSET_SOURCE_BINDING_CONTEXT: &str =
    "maincopy publication asset source binding v1";
const ASSET_RESOLUTION_POLICY_BINDING_CONTEXT: &str = "maincopy asset resolution policy binding v1";
const POST_REVISION_CONTEXT: &str = "maincopy post revision digest v1";
const FRONTEND_BUNDLE_CONTEXT: &str = "maincopy frontend bundle digest v1";
const SITE_SNAPSHOT_CONTEXT: &str = "maincopy site snapshot digest v1";

const ASSET_PREFIX: &str = "asset-b3-v1-";
const POST_PREFIX: &str = "post-b3-v1-";
const SITE_PREFIX: &str = "site-b3-v1-";
const DIGEST_HEX_LENGTH: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestKind {
    Asset,
    PostRevision,
    SiteSnapshot,
}

impl DigestKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Asset => ASSET_PREFIX,
            Self::PostRevision => POST_PREFIX,
            Self::SiteSnapshot => SITE_PREFIX,
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DigestParseError {
    #[error("{kind:?} digest must start with {expected}")]
    InvalidPrefix {
        kind: DigestKind,
        expected: &'static str,
    },
    #[error("{kind:?} digest must contain exactly 32 encoded bytes")]
    InvalidLength { kind: DigestKind },
    #[error("{kind:?} digest must use lowercase hexadecimal")]
    InvalidEncoding { kind: DigestKind },
}

macro_rules! public_digest_type {
    ($name:ident, $kind:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name {
            bytes: [u8; 32],
            encoded: Box<str>,
        }

        impl $name {
            pub fn parse(value: &str) -> Result<Self, DigestParseError> {
                let bytes = parse_digest(value, $kind)?;
                Ok(Self {
                    bytes,
                    encoded: value.into(),
                })
            }

            pub fn as_str(&self) -> &str {
                &self.encoded
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.bytes
            }

            fn from_hash(hash: blake3::Hash) -> Self {
                let bytes = *hash.as_bytes();
                let encoded = format!("{}{}", $kind.prefix(), hash.to_hex()).into_boxed_str();
                Self { bytes, encoded }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = DigestParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<SerializerType>(
                &self,
                serializer: SerializerType,
            ) -> Result<SerializerType::Ok, SerializerType::Error>
            where
                SerializerType: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<DeserializerType>(
                deserializer: DeserializerType,
            ) -> Result<Self, DeserializerType::Error>
            where
                DeserializerType: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

public_digest_type!(AssetDigest, DigestKind::Asset);
public_digest_type!(PostRevisionDigest, DigestKind::PostRevision);
public_digest_type!(SiteSnapshotDigest, DigestKind::SiteSnapshot);

/// Canonical typed post content excluding resolver-owned asset-valued fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostContentDigest([u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublicationContentDigest([u8; 32]);

/// A resolver capability binding, never an input to a public content digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PostAssetSourceBinding([u8; 32]);

/// A resolver capability binding, never an input to a public content digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublicationAssetSourceBinding([u8; 32]);

/// A private capability binding for the effective external-asset allowlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AssetResolutionPolicyBinding([u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrontendBundleDigest([u8; 32]);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostRendererVersion {
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitizerVersion {
    V1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteShellRendererVersion {
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostRendererIdentity {
    settings: RendererSettings,
    renderer: PostRendererVersion,
    sanitizer: SanitizerVersion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreInjectionRenderedArticle<'bytes>(&'bytes [u8]);

impl<'bytes> PreInjectionRenderedArticle<'bytes> {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the post renderer becomes the sole constructor in WP 1.4"
        )
    )]
    pub(super) const fn new(bytes: &'bytes [u8]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> &'bytes [u8] {
        self.0
    }
}

impl PostRendererIdentity {
    pub const fn new(
        settings: RendererSettings,
        renderer: PostRendererVersion,
        sanitizer: SanitizerVersion,
    ) -> Self {
        Self {
            settings,
            renderer,
            sanitizer,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteShellRendererIdentity {
    renderer: SiteShellRendererVersion,
    frontend_bundle: FrontendBundleDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreInjectionSiteShell<'bytes>(&'bytes [u8]);

impl<'bytes> PreInjectionSiteShell<'bytes> {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the site renderer becomes the sole constructor in WP 1.4"
        )
    )]
    pub(super) const fn new(bytes: &'bytes [u8]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> &'bytes [u8] {
        self.0
    }
}

impl SiteShellRendererIdentity {
    pub const fn new(
        renderer: SiteShellRendererVersion,
        frontend_bundle: FrontendBundleDigest,
    ) -> Self {
        Self {
            renderer,
            frontend_bundle,
        }
    }
}

/// Every component is required, even when an asset collection is empty.
pub struct PostRevisionInput<'input> {
    document: &'input PostDocument,
    assets: &'input ResolvedPostAssets,
    site_assets: Option<&'input ResolvedSiteAssets>,
    renderer: &'input PostRendererIdentity,
    pre_injection_article: PreInjectionRenderedArticle<'input>,
    generated_assets: &'input [DigestedAsset],
    effective_distribution: &'input DistributionSettings,
}

impl<'input> PostRevisionInput<'input> {
    pub const fn new(
        document: &'input PostDocument,
        assets: &'input ResolvedPostAssets,
        site_assets: &'input ResolvedSiteAssets,
        renderer: &'input PostRendererIdentity,
        pre_injection_article: PreInjectionRenderedArticle<'input>,
        generated_assets: &'input [DigestedAsset],
        effective_distribution: &'input DistributionSettings,
    ) -> Self {
        Self {
            document,
            assets,
            site_assets: Some(site_assets),
            renderer,
            pre_injection_article,
            generated_assets,
            effective_distribution,
        }
    }

    #[cfg(test)]
    const fn new_unchecked(
        document: &'input PostDocument,
        assets: &'input ResolvedPostAssets,
        renderer: &'input PostRendererIdentity,
        pre_injection_article: PreInjectionRenderedArticle<'input>,
        generated_assets: &'input [DigestedAsset],
        effective_distribution: &'input DistributionSettings,
    ) -> Self {
        Self {
            document,
            assets,
            site_assets: None,
            renderer,
            pre_injection_article,
            generated_assets,
            effective_distribution,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedPostRevision {
    post_id: PostId,
    revision: PostRevisionDigest,
    published_at: OffsetDateTime,
}

impl PublishedPostRevision {
    pub fn new(
        post_id: PostId,
        revision: PostRevisionDigest,
        published_at: OffsetDateTime,
    ) -> Self {
        Self {
            post_id,
            revision,
            published_at: published_at.to_offset(UtcOffset::UTC),
        }
    }

    pub const fn post_id(&self) -> &PostId {
        &self.post_id
    }

    pub const fn revision(&self) -> &PostRevisionDigest {
        &self.revision
    }

    pub const fn published_at(&self) -> OffsetDateTime {
        self.published_at
    }
}

/// Every component is required, including the public publication-ledger view.
pub struct SiteSnapshotInput<'input> {
    publication: &'input PublicationSettings,
    assets: &'input ResolvedSiteAssets,
    renderer: &'input SiteShellRendererIdentity,
    pre_injection_shell: PreInjectionSiteShell<'input>,
    public_posts: &'input [PublishedPostRevision],
}

impl<'input> SiteSnapshotInput<'input> {
    pub const fn new(
        publication: &'input PublicationSettings,
        assets: &'input ResolvedSiteAssets,
        renderer: &'input SiteShellRendererIdentity,
        pre_injection_shell: PreInjectionSiteShell<'input>,
        public_posts: &'input [PublishedPostRevision],
    ) -> Self {
        Self {
            publication,
            assets,
            renderer,
            pre_injection_shell,
            public_posts,
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RevisionIdentityError {
    #[error("resolved asset inputs are bound to different {target:?} content")]
    ResolvedAssetBindingMismatch { target: AssetBindingTarget },
    #[error("post assets were approved under a different external-asset policy")]
    ResolvedAssetPolicyMismatch,
    #[error("asset reference is duplicated: {value}")]
    DuplicateAssetReference { value: String },
    #[error("generated asset path is duplicated: {path}")]
    DuplicateGeneratedAsset { path: String },
    #[error("effective asset origin is duplicated: {origin}")]
    DuplicateAllowedOrigin { origin: String },
    #[error("public post identity is duplicated: {post_id}")]
    DuplicatePublicPost { post_id: PostId },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetBindingTarget {
    Post,
    Publication,
}

pub fn digest_asset(bytes: &[u8]) -> AssetDigest {
    let mut transcript = Transcript::new(ASSET_CONTEXT, b"maincopy-asset", 1);
    transcript.bytes(bytes);
    AssetDigest::from_hash(transcript.finish())
}

pub fn digest_post_content(document: &PostDocument) -> PostContentDigest {
    let mut transcript = Transcript::new(POST_CONTENT_CONTEXT, b"maincopy-post-content", 1);
    encode_post_document(&mut transcript, document);
    PostContentDigest(*transcript.finish().as_bytes())
}

pub(crate) fn digest_publication_content(
    publication: &PublicationSettings,
) -> PublicationContentDigest {
    let mut transcript = Transcript::new(
        PUBLICATION_CONTENT_CONTEXT,
        b"maincopy-publication-content",
        1,
    );
    encode_publication(&mut transcript, publication);
    PublicationContentDigest(*transcript.finish().as_bytes())
}

pub(super) fn bind_post_asset_source(document: &PostDocument) -> PostAssetSourceBinding {
    let canonical_content = digest_post_content(document);
    let mut transcript = Transcript::new(
        POST_ASSET_SOURCE_BINDING_CONTEXT,
        b"maincopy-post-asset-source-binding",
        1,
    );
    transcript.fixed_bytes(&canonical_content.0);
    transcript.optional(document.metadata().image(), |transcript, image| {
        transcript.string(image.as_str());
    });
    PostAssetSourceBinding(*transcript.finish().as_bytes())
}

pub(super) fn bind_publication_asset_source(
    publication: &PublicationSettings,
) -> PublicationAssetSourceBinding {
    let canonical_content = digest_publication_content(publication);
    let mut transcript = Transcript::new(
        PUBLICATION_ASSET_SOURCE_BINDING_CONTEXT,
        b"maincopy-publication-asset-source-binding",
        1,
    );
    transcript.fixed_bytes(&canonical_content.0);
    transcript.optional(publication.site().favicon(), |transcript, favicon| {
        transcript.string(favicon.as_str());
    });
    let authored_origins = publication.assets().allowed_https_origins();
    transcript.sequence_len(authored_origins.len());
    for origin in authored_origins {
        transcript.string(origin.as_str());
    }
    PublicationAssetSourceBinding(*transcript.finish().as_bytes())
}

pub(super) fn bind_asset_resolution_policy(
    allowed_origins: &[ExternalAssetOrigin],
) -> AssetResolutionPolicyBinding {
    let mut origins: Vec<_> = allowed_origins.iter().collect();
    origins.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut transcript = Transcript::new(
        ASSET_RESOLUTION_POLICY_BINDING_CONTEXT,
        b"maincopy-asset-resolution-policy-binding",
        1,
    );
    transcript.sequence_len(origins.len());
    for origin in origins {
        transcript.string(origin.as_str());
    }
    AssetResolutionPolicyBinding(*transcript.finish().as_bytes())
}

pub fn digest_frontend_bundle(bytes: &[u8]) -> FrontendBundleDigest {
    let mut transcript = Transcript::new(FRONTEND_BUNDLE_CONTEXT, b"maincopy-frontend-bundle", 1);
    transcript.bytes(bytes);
    FrontendBundleDigest(*transcript.finish().as_bytes())
}

pub fn digest_post_revision(
    input: &PostRevisionInput<'_>,
) -> Result<PostRevisionDigest, RevisionIdentityError> {
    let referenced_assets = sorted_asset_references(input.assets.references())?;
    let generated_assets = sorted_generated_assets(input.generated_assets)?;
    let content = digest_post_content(input.document);
    if input.assets.source_binding() != &bind_post_asset_source(input.document) {
        return Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
            target: AssetBindingTarget::Post,
        });
    }
    if input
        .site_assets
        .is_some_and(|site_assets| input.assets.policy_binding() != site_assets.policy_binding())
    {
        return Err(RevisionIdentityError::ResolvedAssetPolicyMismatch);
    }
    let mut transcript = Transcript::new(POST_REVISION_CONTEXT, b"maincopy-post-revision", 1);
    transcript.fixed_bytes(&content.0);
    transcript.optional(input.assets.image(), encode_asset_reference);
    encode_asset_references(&mut transcript, &referenced_assets);
    encode_post_renderer(&mut transcript, input.renderer);
    transcript.bytes(input.pre_injection_article.as_bytes());
    encode_generated_assets(&mut transcript, &generated_assets);
    encode_distribution(&mut transcript, input.effective_distribution);
    Ok(PostRevisionDigest::from_hash(transcript.finish()))
}

pub fn digest_site_snapshot(
    input: &SiteSnapshotInput<'_>,
) -> Result<SiteSnapshotDigest, RevisionIdentityError> {
    let site_assets = sorted_asset_references(input.assets.references())?;
    let allowed_origins = sorted_allowed_origins(input.assets.allowed_origins())?;
    let public_posts = sorted_public_posts(input.public_posts)?;
    let publication_content = digest_publication_content(input.publication);
    if input.assets.source_binding() != &bind_publication_asset_source(input.publication) {
        return Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
            target: AssetBindingTarget::Publication,
        });
    }
    let mut transcript = Transcript::new(SITE_SNAPSHOT_CONTEXT, b"maincopy-site-snapshot", 1);
    transcript.fixed_bytes(&publication_content.0);
    transcript.optional(input.assets.favicon(), encode_asset_reference);
    transcript.sequence_len(allowed_origins.len());
    for origin in allowed_origins {
        transcript.string(origin.as_str());
    }
    encode_asset_references(&mut transcript, &site_assets);
    encode_site_renderer(&mut transcript, input.renderer);
    transcript.bytes(input.pre_injection_shell.as_bytes());
    transcript.sequence_len(public_posts.len());
    for post in public_posts {
        transcript.fixed_bytes(post.post_id.as_uuid().as_bytes());
        transcript.fixed_bytes(post.revision.as_bytes());
        transcript.utc_timestamp(post.published_at);
    }
    Ok(SiteSnapshotDigest::from_hash(transcript.finish()))
}

fn parse_digest(value: &str, kind: DigestKind) -> Result<[u8; 32], DigestParseError> {
    let Some(hex) = value.strip_prefix(kind.prefix()) else {
        return Err(DigestParseError::InvalidPrefix {
            kind,
            expected: kind.prefix(),
        });
    };
    if hex.len() != DIGEST_HEX_LENGTH {
        return Err(DigestParseError::InvalidLength { kind });
    }
    if !hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DigestParseError::InvalidEncoding { kind });
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = decode_nibble(pair[0]).ok_or(DigestParseError::InvalidEncoding { kind })?;
        let low = decode_nibble(pair[1]).ok_or(DigestParseError::InvalidEncoding { kind })?;
        bytes[index] = high << 4 | low;
    }
    Ok(bytes)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

struct Transcript(Hasher);

impl Transcript {
    fn new(context: &'static str, kind: &[u8], version: u16) -> Self {
        let mut transcript = Self(Hasher::new_derive_key(context));
        transcript.bytes(kind);
        transcript.0.update(&version.to_be_bytes());
        transcript
    }

    fn finish(self) -> blake3::Hash {
        self.0.finalize()
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn fixed_bytes(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn sequence_len(&mut self, length: usize) {
        self.0.update(&(length as u64).to_be_bytes());
    }

    fn tag(&mut self, tag: u8) {
        self.0.update(&[tag]);
    }

    fn optional<T>(&mut self, value: Option<T>, encode: impl FnOnce(&mut Self, T)) {
        match value {
            Some(value) => {
                self.tag(1);
                encode(self, value);
            }
            None => self.tag(0),
        }
    }

    fn authored_timestamp(&mut self, timestamp: OffsetDateTime) {
        self.0.update(&timestamp.unix_timestamp().to_be_bytes());
        self.0.update(&timestamp.nanosecond().to_be_bytes());
        self.0
            .update(&timestamp.offset().whole_seconds().to_be_bytes());
    }

    fn utc_timestamp(&mut self, timestamp: OffsetDateTime) {
        let timestamp = timestamp.to_offset(UtcOffset::UTC);
        self.0.update(&timestamp.unix_timestamp().to_be_bytes());
        self.0.update(&timestamp.nanosecond().to_be_bytes());
    }
}

fn encode_post_document(transcript: &mut Transcript, document: &PostDocument) {
    let metadata = document.metadata();
    transcript.fixed_bytes(metadata.id().as_uuid().as_bytes());
    transcript.string(metadata.title().as_str());
    transcript.string(metadata.slug().as_str());
    transcript.authored_timestamp(metadata.authored_at());
    transcript.optional(metadata.updated_at(), Transcript::authored_timestamp);
    transcript.string(metadata.description().as_str());
    // Asset-valued fields are excluded here. The final revision transcript
    // requires resolver-owned normalized asset inputs separately.
    transcript.sequence_len(metadata.tags().len());
    for tag in metadata.tags() {
        transcript.string(tag.as_str());
    }
    transcript.sequence_len(metadata.aliases().len());
    for alias in metadata.aliases() {
        transcript.string(alias.as_str());
    }
    transcript.tag(match metadata.draft() {
        DraftStatus::Publishable => 0,
        DraftStatus::Draft => 1,
    });
    transcript.tag(match metadata.tips() {
        PostTipPolicy::InheritPublication => 0,
        PostTipPolicy::Enabled => 1,
        PostTipPolicy::Disabled => 2,
    });
    encode_distribution(transcript, metadata.distribution());
    transcript.bytes(document.markdown().as_str().as_bytes());
}

fn encode_distribution(transcript: &mut Transcript, distribution: &DistributionSettings) {
    let x = distribution.x();
    transcript.tag(match x.mode() {
        DistributionMode::Disabled => 0,
        DistributionMode::Enabled => 1,
    });
    transcript.optional(x.copy(), |transcript, copy| {
        transcript.string(copy.as_str());
    });
}

fn encode_renderer_settings(transcript: &mut Transcript, renderer: RendererSettings) {
    transcript.tag(match renderer.markdown() {
        MarkdownDialect::CommonMark => 0,
    });
    transcript.tag(match renderer.raw_html() {
        RawHtmlPolicy::Disabled => 0,
    });
    transcript.tag(match renderer.code() {
        CodeRenderingMode::EscapedPlainText => 0,
    });
    transcript.tag(match renderer.mermaid() {
        MermaidRenderingMode::Placeholder => 0,
    });
}

fn encode_post_renderer(transcript: &mut Transcript, renderer: &PostRendererIdentity) {
    encode_renderer_settings(transcript, renderer.settings);
    transcript.tag(match renderer.renderer {
        PostRendererVersion::V1 => 0,
    });
    transcript.tag(match renderer.sanitizer {
        SanitizerVersion::V1 => 0,
    });
}

fn encode_site_renderer(transcript: &mut Transcript, renderer: &SiteShellRendererIdentity) {
    transcript.tag(match renderer.renderer {
        SiteShellRendererVersion::V1 => 0,
    });
    transcript.fixed_bytes(&renderer.frontend_bundle.0);
}

fn encode_publication(transcript: &mut Transcript, publication: &PublicationSettings) {
    let site = publication.site();
    transcript.string(site.title().as_str());
    transcript.string(site.base_url().as_str());
    transcript.string(site.description().as_str());
    // Asset-valued fields are excluded here. The final site transcript
    // requires resolver-owned favicon, allowlist, and reference inputs.
    transcript.string(publication.author().name().as_str());

    match publication.subscriptions() {
        SubscriptionSettings::Disabled => transcript.tag(0),
        SubscriptionSettings::Enabled {
            privacy_policy_revision,
        } => {
            transcript.tag(1);
            transcript.string(privacy_policy_revision.as_str());
        }
    }

    match publication.tips() {
        PublicationTipSettings::Unconfigured => transcript.tag(0),
        PublicationTipSettings::Configured { default, range } => {
            transcript.tag(1);
            transcript.tag(match default {
                DefaultPostTipPolicy::Enabled => 0,
                DefaultPostTipPolicy::Disabled => 1,
            });
            transcript.0.update(&range.minimum().get().to_be_bytes());
            transcript.0.update(&range.maximum().get().to_be_bytes());
        }
    }
    encode_renderer_settings(transcript, publication.renderer());
}

fn sorted_asset_references(
    references: &[AssetRevisionReference],
) -> Result<Vec<&AssetRevisionReference>, RevisionIdentityError> {
    let mut references: Vec<_> = references.iter().collect();
    references.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
    for pair in references.windows(2) {
        if pair[0].sort_key() == pair[1].sort_key() {
            let (kind, value) = pair[0].sort_key();
            return Err(RevisionIdentityError::DuplicateAssetReference {
                value: format!("{kind}:{value}"),
            });
        }
    }
    Ok(references)
}

fn encode_asset_references(transcript: &mut Transcript, references: &[&AssetRevisionReference]) {
    transcript.sequence_len(references.len());
    for reference in references {
        encode_asset_reference(transcript, reference);
    }
}

fn encode_asset_reference(transcript: &mut Transcript, reference: &AssetRevisionReference) {
    match reference {
        AssetRevisionReference::Local(asset) => {
            transcript.tag(0);
            transcript.string(asset.path().as_str());
            transcript.fixed_bytes(asset.digest().as_bytes());
        }
        AssetRevisionReference::External(url) => {
            transcript.tag(1);
            transcript.string(url.as_str());
        }
    }
}

fn sorted_allowed_origins(
    origins: &[ExternalAssetOrigin],
) -> Result<Vec<&ExternalAssetOrigin>, RevisionIdentityError> {
    let mut origins: Vec<_> = origins.iter().collect();
    origins.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    for pair in origins.windows(2) {
        if pair[0] == pair[1] {
            return Err(RevisionIdentityError::DuplicateAllowedOrigin {
                origin: pair[0].as_str().to_owned(),
            });
        }
    }
    Ok(origins)
}

fn sorted_generated_assets(
    assets: &[DigestedAsset],
) -> Result<Vec<&DigestedAsset>, RevisionIdentityError> {
    let mut assets: Vec<_> = assets.iter().collect();
    assets.sort_by(|left, right| left.path().as_str().cmp(right.path().as_str()));
    for pair in assets.windows(2) {
        if pair[0].path() == pair[1].path() {
            return Err(RevisionIdentityError::DuplicateGeneratedAsset {
                path: pair[0].path().as_str().to_owned(),
            });
        }
    }
    Ok(assets)
}

fn encode_generated_assets(transcript: &mut Transcript, assets: &[&DigestedAsset]) {
    transcript.sequence_len(assets.len());
    for asset in assets {
        transcript.string(asset.path().as_str());
        transcript.fixed_bytes(asset.digest().as_bytes());
    }
}

fn sorted_public_posts(
    posts: &[PublishedPostRevision],
) -> Result<Vec<&PublishedPostRevision>, RevisionIdentityError> {
    let mut posts: Vec<_> = posts.iter().collect();
    posts.sort_by(|left, right| left.post_id.cmp(&right.post_id));
    for pair in posts.windows(2) {
        if pair[0].post_id == pair[1].post_id {
            return Err(RevisionIdentityError::DuplicatePublicPost {
                post_id: pair[0].post_id.clone(),
            });
        }
    }
    Ok(posts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ExternalAssetUrl, PostSource, PublicationSource, validate_content};

    const PUBLICATION: &str = r#"
[site]
title = "Example"
base_url = "https://example.com/"
description = "An example publication."

[author]
name = "Example Author"
"#;

    fn validate_post(frontmatter: &str, markdown: &str) -> (PublicationSettings, PostDocument) {
        let source = format!("+++\n{frontmatter}+++\n{markdown}");
        let content = validate_content(
            PublicationSource::new("publication.toml", PUBLICATION),
            [PostSource::in_posts("posts/example.md", &source)],
        )
        .expect("fixture content must validate");
        (content.publication().clone(), content.posts()[0].clone())
    }

    fn validate_publication(source: &str) -> PublicationSettings {
        validate_content(
            PublicationSource::new("publication.toml", source),
            std::iter::empty::<PostSource<'_>>(),
        )
        .expect("fixture publication must validate")
        .publication()
        .clone()
    }

    fn frontmatter(authored_at: &str) -> String {
        format!(
            "id = \"11111111-1111-4111-8111-111111111111\"\n\
             title = \"Example Post\"\n\
             slug = \"example-post\"\n\
             authored_at = {authored_at}\n\
             description = \"An example post.\"\n\
             tags = [\"Rust\", \"sqlite\"]\n"
        )
    }

    fn renderer() -> PostRendererIdentity {
        PostRendererIdentity::new(
            RendererSettings::baseline(),
            PostRendererVersion::V1,
            SanitizerVersion::V1,
        )
    }

    #[test]
    fn public_digests_require_exact_versioned_lowercase_encodings() {
        for (valid, invalid_kind) in [
            (
                format!("asset-b3-v1-{}", "ab".repeat(32)),
                DigestKind::Asset,
            ),
            (
                format!("post-b3-v1-{}", "ab".repeat(32)),
                DigestKind::PostRevision,
            ),
            (
                format!("site-b3-v1-{}", "ab".repeat(32)),
                DigestKind::SiteSnapshot,
            ),
        ] {
            let parsed = match invalid_kind {
                DigestKind::Asset => {
                    AssetDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
                DigestKind::PostRevision => {
                    PostRevisionDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
                DigestKind::SiteSnapshot => {
                    SiteSnapshotDigest::parse(&valid).map(|value| value.as_str().to_owned())
                }
            };
            assert_eq!(parsed.unwrap(), valid);
        }

        assert!(AssetDigest::parse(&format!("asset-b3-v1-{}", "AB".repeat(32))).is_err());
        assert!(PostRevisionDigest::parse(&format!("post-b3-v1-{}", "aa".repeat(31))).is_err());
        assert!(SiteSnapshotDigest::parse(&format!("post-b3-v1-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-b3-v2-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-sha256-v1-{}", "aa".repeat(32))).is_err());
        assert!(AssetDigest::parse(&format!("asset-b3-v1-{}", "gg".repeat(32))).is_err());

        let value = format!("post-b3-v1-{}", "12".repeat(32));
        let digest = PostRevisionDigest::parse(&value).unwrap();
        assert_eq!(serde_json::to_value(&digest).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<PostRevisionDigest>(serde_json::json!(value)).unwrap(),
            digest
        );
    }

    #[test]
    fn identity_enum_wire_names_are_stable() {
        for (value, expected) in [
            (serde_json::to_value(DigestKind::Asset).unwrap(), "asset"),
            (
                serde_json::to_value(DigestKind::PostRevision).unwrap(),
                "post_revision",
            ),
            (
                serde_json::to_value(DigestKind::SiteSnapshot).unwrap(),
                "site_snapshot",
            ),
            (serde_json::to_value(PostRendererVersion::V1).unwrap(), "v1"),
            (serde_json::to_value(SanitizerVersion::V1).unwrap(), "v1"),
            (
                serde_json::to_value(SiteShellRendererVersion::V1).unwrap(),
                "v1",
            ),
            (
                serde_json::to_value(AssetBindingTarget::Post).unwrap(),
                "post",
            ),
            (
                serde_json::to_value(AssetBindingTarget::Publication).unwrap(),
                "publication",
            ),
        ] {
            assert_eq!(value, serde_json::json!(expected));
        }
    }

    #[test]
    fn typed_post_content_is_canonical_but_preserves_markdown_and_authored_offset() {
        let first_frontmatter = frontmatter("2026-08-29T12:00:00-04:00");
        let reordered = first_frontmatter
            .replace(
                "title = \"Example Post\"\nslug = \"example-post\"",
                "slug = \"example-post\"\n# equivalent comment\ntitle = 'Example Post'",
            )
            .replace(
                "tags = [\"Rust\", \"sqlite\"]",
                "tags = [\"rust\", \"sqlite\"]",
            )
            + "draft = false\n[distribution.x]\nenabled = false\n";
        let (_, first) = validate_post(&first_frontmatter, "# Body\n");
        let (_, equivalent) = validate_post(&reordered, "# Body\n");
        let (_, offset_changed) = validate_post(&frontmatter("2026-08-29T16:00:00Z"), "# Body\n");
        let (_, markdown_changed) = validate_post(&first_frontmatter, "# Body\r\n");

        assert_eq!(
            digest_post_content(&first),
            digest_post_content(&equivalent)
        );
        assert_ne!(
            digest_post_content(&first),
            digest_post_content(&offset_changed)
        );
        assert_ne!(
            digest_post_content(&first),
            digest_post_content(&markdown_changed)
        );
    }

    #[test]
    fn every_canonical_post_field_and_authored_order_is_identity_bearing() {
        let baseline_frontmatter = frontmatter("2026-08-29T12:00:00Z");
        let (_, baseline_post) = validate_post(&baseline_frontmatter, "# Body\n");
        let baseline = digest_post_content(&baseline_post);
        let variants = [
            (
                "id",
                baseline_frontmatter.replace(
                    "11111111-1111-4111-8111-111111111111",
                    "22222222-2222-4222-8222-222222222222",
                ),
            ),
            (
                "title",
                baseline_frontmatter.replace("Example Post", "Changed Post"),
            ),
            (
                "slug",
                baseline_frontmatter.replace("example-post", "changed-post"),
            ),
            (
                "updated_at",
                format!("{baseline_frontmatter}updated_at = 2026-08-30T12:00:00Z\n"),
            ),
            (
                "description",
                baseline_frontmatter.replace("An example post.", "A changed post."),
            ),
            (
                "tags",
                baseline_frontmatter.replace(
                    "tags = [\"Rust\", \"sqlite\"]",
                    "tags = [\"Rust\", \"wal\"]",
                ),
            ),
            (
                "aliases",
                format!("{baseline_frontmatter}aliases = [\"old-example\"]\n"),
            ),
            ("draft", format!("{baseline_frontmatter}draft = true\n")),
            ("tips", format!("{baseline_frontmatter}tips = false\n")),
            (
                "distribution",
                format!(
                    "{baseline_frontmatter}[distribution.x]\nenabled = true\ntext = \"Share this\"\n"
                ),
            ),
        ];
        for (field, frontmatter) in variants {
            let (_, changed) = validate_post(&frontmatter, "# Body\n");
            assert_ne!(
                baseline,
                digest_post_content(&changed),
                "{field} was omitted from canonical post identity"
            );
        }

        let ordered = format!("{baseline_frontmatter}aliases = [\"first\", \"second\"]\n");
        let reversed_aliases = format!("{baseline_frontmatter}aliases = [\"second\", \"first\"]\n");
        let reversed_tags = baseline_frontmatter.replace(
            "tags = [\"Rust\", \"sqlite\"]",
            "tags = [\"sqlite\", \"Rust\"]",
        );
        let (_, ordered) = validate_post(&ordered, "# Body\n");
        let (_, reversed_aliases) = validate_post(&reversed_aliases, "# Body\n");
        let (_, reversed_tags) = validate_post(&reversed_tags, "# Body\n");
        assert_ne!(
            digest_post_content(&ordered),
            digest_post_content(&reversed_aliases)
        );
        assert_ne!(baseline, digest_post_content(&reversed_tags));
    }

    #[test]
    fn post_revision_requires_and_hashes_every_component() {
        let (publication, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let local = DigestedAsset::new(
            super::super::LogicalAssetPath::parse("assets/cover.webp").unwrap(),
            digest_asset(b"cover"),
        );
        let refs = [AssetRevisionReference::local(local.clone())];
        let generated = [DigestedAsset::new(
            super::super::LogicalAssetPath::parse("assets/generated/diagram.png").unwrap(),
            digest_asset(b"diagram"),
        )];
        let renderer = renderer();
        let baseline = digest_post_revision(&PostRevisionInput::new_unchecked(
            &post,
            &ResolvedPostAssets::new(&post, None, refs.to_vec()),
            &renderer,
            PreInjectionRenderedArticle::new(b"<h1>Body</h1>"),
            &generated,
            post.metadata().distribution(),
        ))
        .unwrap();
        let changed = digest_post_revision(&PostRevisionInput::new_unchecked(
            &post,
            &ResolvedPostAssets::new(&post, None, refs.to_vec()),
            &renderer,
            PreInjectionRenderedArticle::new(b"<h1>Changed</h1>"),
            &generated,
            post.metadata().distribution(),
        ))
        .unwrap();
        assert_ne!(baseline, changed);
        assert_eq!(
            baseline.as_str(),
            "post-b3-v1-bd78c5db53768b8da38359df826a6c928a1a21b764fcfb44cef99d43a9da8a7b"
        );

        let duplicate_refs = [
            AssetRevisionReference::local(local.clone()),
            AssetRevisionReference::local(local),
        ];
        assert!(matches!(
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &post,
                &ResolvedPostAssets::new(&post, None, duplicate_refs.to_vec()),
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            )),
            Err(RevisionIdentityError::DuplicateAssetReference { .. })
        ));
        drop(publication);
    }

    #[test]
    fn asset_and_collection_order_cannot_change_revision_identity() {
        let (_, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let first = AssetRevisionReference::local(DigestedAsset::new(
            super::super::LogicalAssetPath::parse("assets/a.bin").unwrap(),
            digest_asset(b"a"),
        ));
        let second = AssetRevisionReference::external(
            super::super::ExternalAssetUrl::parse("https://cdn.example/b.bin").unwrap(),
        );
        let renderer = renderer();
        let forward = [first.clone(), second.clone()];
        let reverse = [second, first];

        let digest = |assets: &[AssetRevisionReference]| {
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &post,
                &ResolvedPostAssets::new(&post, None, assets.to_vec()),
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            ))
            .unwrap()
        };
        assert_eq!(digest(&forward), digest(&reverse));
    }

    #[test]
    fn generated_assets_and_effective_distribution_are_complete_components() {
        let baseline_frontmatter = frontmatter("2026-08-29T12:00:00Z");
        let (_, post) = validate_post(&baseline_frontmatter, "# Body\n");
        let distributed_frontmatter = format!(
            "{baseline_frontmatter}[distribution.x]\nenabled = true\ntext = \"Share this\"\n"
        );
        let (_, distributed) = validate_post(&distributed_frontmatter, "# Body\n");
        let renderer = renderer();
        let first = DigestedAsset::new(
            super::super::LogicalAssetPath::parse("assets/generated/a.bin").unwrap(),
            digest_asset(b"first"),
        );
        let second = DigestedAsset::new(
            super::super::LogicalAssetPath::parse("assets/generated/b.bin").unwrap(),
            digest_asset(b"second"),
        );
        let digest = |generated: &[DigestedAsset], distribution: &DistributionSettings| {
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &post,
                &ResolvedPostAssets::new(&post, None, Vec::new()),
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                generated,
                distribution,
            ))
        };
        let baseline = digest(
            &[first.clone(), second.clone()],
            post.metadata().distribution(),
        )
        .unwrap();
        assert_eq!(
            baseline,
            digest(
                &[second.clone(), first.clone()],
                post.metadata().distribution()
            )
            .unwrap()
        );
        assert_ne!(
            baseline,
            digest(
                &[
                    DigestedAsset::new(first.path().clone(), digest_asset(b"changed")),
                    second.clone()
                ],
                post.metadata().distribution()
            )
            .unwrap()
        );
        assert_ne!(
            baseline,
            digest(
                &[
                    DigestedAsset::new(
                        super::super::LogicalAssetPath::parse("assets/generated/renamed.bin")
                            .unwrap(),
                        first.digest().clone(),
                    ),
                    second.clone(),
                ],
                post.metadata().distribution()
            )
            .unwrap()
        );
        assert_ne!(
            baseline,
            digest(
                &[first.clone(), second.clone()],
                distributed.metadata().distribution()
            )
            .unwrap()
        );
        assert!(matches!(
            digest(
                &[first.clone(), first.clone()],
                post.metadata().distribution()
            ),
            Err(RevisionIdentityError::DuplicateGeneratedAsset { .. })
        ));

        let digest_with_unreferenced_catalog_asset = |_asset: &AssetDigest| {
            digest(
                &[first.clone(), second.clone()],
                post.metadata().distribution(),
            )
            .unwrap()
        };
        assert_eq!(
            digest_with_unreferenced_catalog_asset(&digest_asset(b"not referenced")),
            digest_with_unreferenced_catalog_asset(&digest_asset(b"other bytes"))
        );
    }

    #[test]
    fn local_asset_path_bytes_and_external_url_are_separate_revision_inputs() {
        let (_, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let renderer = renderer();
        let digest = |assets: &[AssetRevisionReference]| {
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &post,
                &ResolvedPostAssets::new(&post, None, assets.to_vec()),
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            ))
            .unwrap()
        };
        let local = |path: &str, bytes: &[u8]| {
            AssetRevisionReference::local(DigestedAsset::new(
                super::super::LogicalAssetPath::parse(path).unwrap(),
                digest_asset(bytes),
            ))
        };
        let baseline = [local("assets/a.bin", b"same")];
        let changed_path = [local("assets/b.bin", b"same")];
        let changed_bytes = [local("assets/a.bin", b"changed")];
        let external_v1 = [AssetRevisionReference::external(
            super::super::ExternalAssetUrl::parse("https://cdn.example/a.bin?v=1").unwrap(),
        )];
        let external_v2 = [AssetRevisionReference::external(
            super::super::ExternalAssetUrl::parse("https://cdn.example/a.bin?v=2").unwrap(),
        )];

        assert_ne!(digest(&baseline), digest(&changed_path));
        assert_ne!(digest(&baseline), digest(&changed_bytes));
        assert_ne!(digest(&external_v1), digest(&external_v2));
    }

    #[test]
    fn resolved_image_normalization_is_stable_and_image_identity_is_required() {
        let first_frontmatter = format!(
            "{}image = \"HTTPS://CDN.EXAMPLE:443/image.png\"\n",
            frontmatter("2026-08-29T12:00:00Z")
        );
        let second_frontmatter = format!(
            "{}image = \"https://cdn.example/image.png\"\n",
            frontmatter("2026-08-29T12:00:00Z")
        );
        let (_, first) = validate_post(&first_frontmatter, "# Body\n");
        let (_, second) = validate_post(&second_frontmatter, "# Body\n");
        let image = AssetRevisionReference::external(
            super::super::ExternalAssetUrl::parse("https://cdn.example/image.png").unwrap(),
        );
        let first_resolved = ResolvedPostAssets::new(&first, Some(image.clone()), Vec::new());
        let second_resolved = ResolvedPostAssets::new(&second, Some(image), Vec::new());
        let unresolved = ResolvedPostAssets::new(&first, None, Vec::new());
        let renderer = renderer();
        let digest = |post: &PostDocument, assets: &ResolvedPostAssets| {
            digest_post_revision(&PostRevisionInput::new_unchecked(
                post,
                assets,
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            ))
            .unwrap()
        };

        assert_eq!(
            digest(&first, &first_resolved),
            digest(&second, &second_resolved)
        );
        assert_ne!(digest(&first, &first_resolved), digest(&first, &unresolved));
    }

    #[test]
    fn length_frames_prevent_adjacent_field_ambiguity() {
        let hash = |parts: &[&[u8]]| {
            let mut transcript = Transcript::new(
                "maincopy transcript boundary test v1",
                b"maincopy-boundary-test",
                1,
            );
            for part in parts {
                transcript.bytes(part);
            }
            transcript.finish()
        };

        assert_ne!(
            hash(&[b"ab".as_slice(), b"c".as_slice()]),
            hash(&[b"a".as_slice(), b"bc".as_slice()])
        );
    }

    #[test]
    fn site_identity_sorts_ledger_entries_and_normalizes_operational_time() {
        let (publication, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let renderer = renderer();
        let revision = digest_post_revision(&PostRevisionInput::new_unchecked(
            &post,
            &ResolvedPostAssets::new(&post, None, Vec::new()),
            &renderer,
            PreInjectionRenderedArticle::new(b"rendered"),
            &[],
            post.metadata().distribution(),
        ))
        .unwrap();
        let first = PublishedPostRevision::new(
            PostId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
            revision.clone(),
            OffsetDateTime::from_unix_timestamp(1_777_734_400)
                .unwrap()
                .to_offset(time::UtcOffset::from_hms(-4, 0, 0).unwrap()),
        );
        let second = PublishedPostRevision::new(
            PostId::parse("22222222-2222-4222-8222-222222222222").unwrap(),
            revision,
            OffsetDateTime::from_unix_timestamp(1_777_734_400).unwrap(),
        );
        let site_renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle"),
        );
        let forward = [first.clone(), second.clone()];
        let reverse = [second, first.clone()];
        let digest = |posts: &[PublishedPostRevision]| {
            digest_site_snapshot(&SiteSnapshotInput::new(
                &publication,
                &ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new()),
                &site_renderer,
                PreInjectionSiteShell::new(b"shell"),
                posts,
            ))
            .unwrap()
        };
        assert_eq!(digest(&forward), digest(&reverse));
        assert_eq!(
            digest(&forward).as_str(),
            "site-b3-v1-449c3916eb79c184424c8b7dde4b4bfe3836703a1e75175e9003cc1a70973cb9"
        );

        let duplicate = [first.clone(), first];
        assert!(matches!(
            digest_site_snapshot(&SiteSnapshotInput::new(
                &publication,
                &ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new()),
                &site_renderer,
                PreInjectionSiteShell::new(b"shell"),
                &duplicate,
            )),
            Err(RevisionIdentityError::DuplicatePublicPost { .. })
        ));
    }

    #[test]
    fn renderer_output_public_revision_and_activation_each_change_site_identity() {
        let (publication, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let post_renderer = renderer();
        let revision = digest_post_revision(&PostRevisionInput::new_unchecked(
            &post,
            &ResolvedPostAssets::new(&post, None, Vec::new()),
            &post_renderer,
            PreInjectionRenderedArticle::new(b"rendered"),
            &[],
            post.metadata().distribution(),
        ))
        .unwrap();
        let post_id = PostId::parse("11111111-1111-4111-8111-111111111111").unwrap();
        let published_at = OffsetDateTime::from_unix_timestamp(1_777_734_400).unwrap();
        let baseline_post =
            PublishedPostRevision::new(post_id.clone(), revision.clone(), published_at);
        let changed_revision = PublishedPostRevision::new(
            post_id.clone(),
            PostRevisionDigest::parse(&format!("post-b3-v1-{}", "22".repeat(32))).unwrap(),
            published_at,
        );
        let changed_activation =
            PublishedPostRevision::new(post_id, revision, published_at + time::Duration::SECOND);
        let baseline_renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle-a"),
        );
        let changed_renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle-b"),
        );
        let digest = |renderer: &SiteShellRendererIdentity,
                      shell: &'static [u8],
                      posts: &[PublishedPostRevision]| {
            digest_site_snapshot(&SiteSnapshotInput::new(
                &publication,
                &ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new()),
                renderer,
                PreInjectionSiteShell::new(shell),
                posts,
            ))
            .unwrap()
        };
        let baseline = digest(
            &baseline_renderer,
            b"shell-a",
            std::slice::from_ref(&baseline_post),
        );

        assert_ne!(
            baseline,
            digest(
                &changed_renderer,
                b"shell-a",
                std::slice::from_ref(&baseline_post)
            )
        );
        assert_ne!(
            baseline,
            digest(
                &baseline_renderer,
                b"shell-b",
                std::slice::from_ref(&baseline_post)
            )
        );
        assert_ne!(
            baseline,
            digest(
                &baseline_renderer,
                b"shell-a",
                std::slice::from_ref(&changed_revision)
            )
        );
        assert_ne!(
            baseline,
            digest(
                &baseline_renderer,
                b"shell-a",
                std::slice::from_ref(&changed_activation)
            )
        );
    }

    #[test]
    fn publication_favicon_and_site_references_are_required_site_components() {
        let renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle"),
        );
        let baseline_publication = validate_publication(PUBLICATION);
        let digest = |publication: &PublicationSettings, assets: &ResolvedSiteAssets| {
            digest_site_snapshot(&SiteSnapshotInput::new(
                publication,
                assets,
                &renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            ))
            .unwrap()
        };
        let baseline_assets =
            ResolvedSiteAssets::new(&baseline_publication, None, Vec::new(), Vec::new());
        let baseline = digest(&baseline_publication, &baseline_assets);
        let variants = [
            PUBLICATION.replace("title = \"Example\"", "title = \"Changed\""),
            PUBLICATION.replace(
                "base_url = \"https://example.com/\"",
                "base_url = \"https://changed.example/\"",
            ),
            PUBLICATION.replace("An example publication.", "A changed publication."),
            PUBLICATION.replace("Example Author", "Changed Author"),
            format!(
                "{PUBLICATION}\n[subscriptions]\nenabled = true\nprivacy_policy_revision = \"v2\"\n"
            ),
            format!(
                "{PUBLICATION}\n[tips]\nenabled = true\nminimum_sats = 100\nmaximum_sats = 1000\n"
            ),
        ];
        for source in variants {
            let publication = validate_publication(&source);
            let assets = ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new());
            assert_ne!(baseline, digest(&publication, &assets));
        }

        let local = |path: &str, bytes: &[u8]| {
            AssetRevisionReference::local(DigestedAsset::new(
                super::super::LogicalAssetPath::parse(path).unwrap(),
                digest_asset(bytes),
            ))
        };
        let favicon = local("assets/favicon.png", b"favicon");
        let favicon_changed = local("assets/favicon.png", b"changed favicon");
        let reference = local("assets/site/banner.png", b"banner");
        let reference_changed_path = local("assets/site/renamed.png", b"banner");
        let reference_changed_bytes = local("assets/site/banner.png", b"changed banner");
        let favicon_identity = digest(
            &baseline_publication,
            &ResolvedSiteAssets::new(&baseline_publication, Some(favicon), Vec::new(), Vec::new()),
        );
        let changed_favicon_identity = digest(
            &baseline_publication,
            &ResolvedSiteAssets::new(
                &baseline_publication,
                Some(favicon_changed),
                Vec::new(),
                Vec::new(),
            ),
        );
        let reference_identity = digest(
            &baseline_publication,
            &ResolvedSiteAssets::new(&baseline_publication, None, Vec::new(), vec![reference]),
        );
        let changed_reference_path_identity = digest(
            &baseline_publication,
            &ResolvedSiteAssets::new(
                &baseline_publication,
                None,
                Vec::new(),
                vec![reference_changed_path],
            ),
        );
        let changed_reference_bytes_identity = digest(
            &baseline_publication,
            &ResolvedSiteAssets::new(
                &baseline_publication,
                None,
                Vec::new(),
                vec![reference_changed_bytes],
            ),
        );
        assert_ne!(baseline, favicon_identity);
        assert_ne!(favicon_identity, changed_favicon_identity);
        assert_ne!(baseline, reference_identity);
        assert_ne!(reference_identity, changed_reference_path_identity);
        assert_ne!(reference_identity, changed_reference_bytes_identity);
    }

    #[test]
    fn resolved_asset_capabilities_are_bound_to_their_source_content() {
        let baseline_frontmatter = frontmatter("2026-08-29T12:00:00Z");
        let (_, baseline_post) = validate_post(&baseline_frontmatter, "# Body\n");
        let changed_frontmatter = baseline_frontmatter.replace("Example Post", "Changed Post");
        let (_, changed_post) = validate_post(&changed_frontmatter, "# Body\n");
        let post_assets = ResolvedPostAssets::new(&baseline_post, None, Vec::new());
        let post_renderer = renderer();
        assert!(matches!(
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &changed_post,
                &post_assets,
                &post_renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                changed_post.metadata().distribution(),
            )),
            Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
                target: AssetBindingTarget::Post
            })
        ));

        let image_frontmatter =
            format!("{baseline_frontmatter}image = \"https://cdn.example/a.png\"\n");
        let changed_image_frontmatter =
            image_frontmatter.replace("cdn.example/a.png", "cdn.example/b.png");
        let (_, image_post) = validate_post(&image_frontmatter, "# Body\n");
        let (_, changed_image_post) = validate_post(&changed_image_frontmatter, "# Body\n");
        let image_assets = ResolvedPostAssets::new(
            &image_post,
            Some(AssetRevisionReference::external(
                ExternalAssetUrl::parse("https://cdn.example/a.png").unwrap(),
            )),
            Vec::new(),
        );
        assert!(matches!(
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &changed_image_post,
                &image_assets,
                &post_renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                changed_image_post.metadata().distribution(),
            )),
            Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
                target: AssetBindingTarget::Post
            })
        ));

        let publication = validate_publication(PUBLICATION);
        let changed_publication = validate_publication(
            &PUBLICATION.replace("title = \"Example\"", "title = \"Changed\""),
        );
        let site_assets = ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new());
        let site_renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle"),
        );
        assert!(matches!(
            digest_site_snapshot(&SiteSnapshotInput::new(
                &changed_publication,
                &site_assets,
                &site_renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            )),
            Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
                target: AssetBindingTarget::Publication
            })
        ));

        let favicon_source = PUBLICATION.replace(
            "description = \"An example publication.\"",
            "description = \"An example publication.\"\nfavicon = \"https://cdn.example/a.png\"",
        );
        let changed_favicon_source =
            favicon_source.replace("cdn.example/a.png", "cdn.example/b.png");
        let favicon_publication = validate_publication(&favicon_source);
        let changed_favicon_publication = validate_publication(&changed_favicon_source);
        let favicon_assets = ResolvedSiteAssets::new(
            &favicon_publication,
            Some(AssetRevisionReference::external(
                ExternalAssetUrl::parse("https://cdn.example/a.png").unwrap(),
            )),
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            digest_site_snapshot(&SiteSnapshotInput::new(
                &changed_favicon_publication,
                &favicon_assets,
                &site_renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            )),
            Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
                target: AssetBindingTarget::Publication
            })
        ));

        let origin_source = format!(
            "{PUBLICATION}\n[assets]\nallowed_https_origins = [\"HTTPS://A.EXAMPLE:443\"]\n"
        );
        let equivalent_origin_source =
            format!("{PUBLICATION}\n[assets]\nallowed_https_origins = [\"https://a.example/\"]\n");
        let origin_publication = validate_publication(&origin_source);
        let equivalent_origin_publication = validate_publication(&equivalent_origin_source);
        let origin_assets = ResolvedSiteAssets::new(
            &origin_publication,
            None,
            vec![ExternalAssetOrigin::parse("https://a.example/").unwrap()],
            Vec::new(),
        );
        assert!(matches!(
            digest_site_snapshot(&SiteSnapshotInput::new(
                &equivalent_origin_publication,
                &origin_assets,
                &site_renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            )),
            Err(RevisionIdentityError::ResolvedAssetBindingMismatch {
                target: AssetBindingTarget::Publication
            })
        ));
    }

    #[test]
    fn post_revision_rejects_assets_approved_before_allowlist_revocation() {
        let image_frontmatter = format!(
            "{}image = \"https://cdn.example/cover-v1.png\"\n",
            frontmatter("2026-08-29T12:00:00Z")
        );
        let (publication, post) = validate_post(&image_frontmatter, "# Body\n");
        let allowed = vec![ExternalAssetOrigin::parse("https://cdn.example/").unwrap()];
        let expanded = vec![
            ExternalAssetOrigin::parse("https://cdn.example/").unwrap(),
            ExternalAssetOrigin::parse("https://media.example/").unwrap(),
        ];
        let image = AssetRevisionReference::external(
            ExternalAssetUrl::parse("https://cdn.example/cover-v1.png").unwrap(),
        );
        let approved_assets = ResolvedPostAssets::from_resolution(
            &post,
            &allowed,
            Some(image.clone()),
            Vec::new(),
            Vec::new(),
        );
        let expanded_assets = ResolvedPostAssets::from_resolution(
            &post,
            &expanded,
            Some(image),
            Vec::new(),
            Vec::new(),
        );
        let approved_site = ResolvedSiteAssets::new(&publication, None, allowed, Vec::new());
        let expanded_site = ResolvedSiteAssets::new(&publication, None, expanded, Vec::new());
        let revoked_site = ResolvedSiteAssets::new(&publication, None, Vec::new(), Vec::new());
        let renderer = renderer();
        let digest = |assets: &ResolvedPostAssets, site_assets: &ResolvedSiteAssets| {
            digest_post_revision(&PostRevisionInput::new(
                &post,
                assets,
                site_assets,
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            ))
        };

        let approved = digest(&approved_assets, &approved_site).unwrap();
        assert_eq!(approved, digest(&expanded_assets, &expanded_site).unwrap());
        assert_eq!(
            digest(&approved_assets, &revoked_site),
            Err(RevisionIdentityError::ResolvedAssetPolicyMismatch)
        );
        assert_eq!(
            digest(&approved_assets, &expanded_site),
            Err(RevisionIdentityError::ResolvedAssetPolicyMismatch)
        );
    }

    #[test]
    fn effective_normalized_allowlist_is_order_independent_and_identity_bearing() {
        let authored_first = format!(
            "{PUBLICATION}\n[assets]\nallowed_https_origins = [\"HTTPS://A.EXAMPLE:443\"]\n"
        );
        let authored_equivalent =
            format!("{PUBLICATION}\n[assets]\nallowed_https_origins = [\"https://a.example/\"]\n");
        let publication = validate_publication(&authored_first);
        let equivalent_publication = validate_publication(&authored_equivalent);
        let renderer = SiteShellRendererIdentity::new(
            SiteShellRendererVersion::V1,
            digest_frontend_bundle(b"bundle"),
        );
        let first = ExternalAssetOrigin::parse("https://a.example").unwrap();
        let second = ExternalAssetOrigin::parse("HTTPS://B.EXAMPLE:443/").unwrap();
        let changed = ExternalAssetOrigin::parse("https://c.example/").unwrap();
        let digest = |publication: &PublicationSettings, origins: Vec<ExternalAssetOrigin>| {
            digest_site_snapshot(&SiteSnapshotInput::new(
                publication,
                &ResolvedSiteAssets::new(publication, None, origins, Vec::new()),
                &renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            ))
            .unwrap()
        };

        assert_eq!(
            digest(&publication, vec![first.clone(), second.clone()]),
            digest(&publication, vec![second.clone(), first])
        );
        assert_eq!(
            digest(
                &publication,
                vec![ExternalAssetOrigin::parse("https://a.example").unwrap()]
            ),
            digest(
                &equivalent_publication,
                vec![ExternalAssetOrigin::parse("https://a.example/").unwrap()]
            )
        );
        assert_ne!(
            digest(&publication, vec![second]),
            digest(&publication, vec![changed])
        );
        let duplicate = ExternalAssetOrigin::parse("https://duplicate.example").unwrap();
        assert!(matches!(
            digest_site_snapshot(&SiteSnapshotInput::new(
                &publication,
                &ResolvedSiteAssets::new(
                    &publication,
                    None,
                    vec![duplicate.clone(), duplicate],
                    Vec::new(),
                ),
                &renderer,
                PreInjectionSiteShell::new(b"shell"),
                &[],
            )),
            Err(RevisionIdentityError::DuplicateAllowedOrigin { .. })
        ));
    }

    #[test]
    fn advisory_git_provenance_is_not_a_revision_input() {
        let (_, post) = validate_post(&frontmatter("2026-08-29T12:00:00Z"), "# Body\n");
        let renderer = renderer();
        let digest = |_provenance: Option<&super::super::SourceCommit>| {
            digest_post_revision(&PostRevisionInput::new_unchecked(
                &post,
                &ResolvedPostAssets::new(&post, None, Vec::new()),
                &renderer,
                PreInjectionRenderedArticle::new(b"rendered"),
                &[],
                post.metadata().distribution(),
            ))
            .unwrap()
        };
        let first_commit =
            super::super::SourceCommit::parse(&format!("git-sha1:{}", "11".repeat(20))).unwrap();
        let second_commit =
            super::super::SourceCommit::parse(&format!("git-sha256:{}", "22".repeat(32))).unwrap();

        assert_ne!(first_commit, second_commit);
        assert_eq!(digest(None), digest(Some(&first_commit)));
        assert_eq!(digest(Some(&first_commit)), digest(Some(&second_commit)));
    }

    #[test]
    fn asset_digest_has_a_stable_domain_separated_golden_value() {
        let asset = digest_asset(b"maincopy");
        assert_eq!(
            asset.as_str(),
            "asset-b3-v1-20b8cb3fe0a1f1eae595a39939aef0e08660b7117e462d4d0b4f9510075681ae"
        );
        assert_ne!(asset.as_bytes(), &digest_frontend_bundle(b"maincopy").0);
    }
}
