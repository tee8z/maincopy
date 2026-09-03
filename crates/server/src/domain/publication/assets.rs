use markdown_compiler::LogicalAssetPath;

/// Fixed response behavior selected from a trusted logical asset path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssetDelivery {
    Inline(InlineAssetMediaType),
    Attachment,
}

impl AssetDelivery {
    /// Selects public delivery for an authored asset retained in a snapshot.
    pub(crate) fn for_authored(path: &LogicalAssetPath) -> Self {
        inline_media_type(path).map_or(Self::Attachment, Self::Inline)
    }

    /// Renderer output is inert until a renderer-specific sanitizer grants a
    /// more capable delivery type.
    pub(crate) const fn for_untrusted_generated() -> Self {
        Self::Attachment
    }

    pub(crate) const fn content_type(self) -> &'static str {
        match self {
            Self::Inline(media_type) => media_type.as_str(),
            Self::Attachment => "application/octet-stream",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InlineAssetMediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
    Icon,
    Mp4,
    Webm,
    Mp3,
    Wav,
    Ogg,
    Woff,
    Woff2,
}

impl InlineAssetMediaType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Icon => "image/x-icon",
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
            Self::Ogg => "audio/ogg",
            Self::Woff => "font/woff",
            Self::Woff2 => "font/woff2",
        }
    }
}

fn inline_media_type(path: &LogicalAssetPath) -> Option<InlineAssetMediaType> {
    let extension = path
        .as_str()
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension);
    if extension.eq_ignore_ascii_case("png") {
        Some(InlineAssetMediaType::Png)
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some(InlineAssetMediaType::Jpeg)
    } else if extension.eq_ignore_ascii_case("gif") {
        Some(InlineAssetMediaType::Gif)
    } else if extension.eq_ignore_ascii_case("webp") {
        Some(InlineAssetMediaType::Webp)
    } else if extension.eq_ignore_ascii_case("avif") {
        Some(InlineAssetMediaType::Avif)
    } else if extension.eq_ignore_ascii_case("ico") {
        Some(InlineAssetMediaType::Icon)
    } else if extension.eq_ignore_ascii_case("mp4") {
        Some(InlineAssetMediaType::Mp4)
    } else if extension.eq_ignore_ascii_case("webm") {
        Some(InlineAssetMediaType::Webm)
    } else if extension.eq_ignore_ascii_case("mp3") {
        Some(InlineAssetMediaType::Mp3)
    } else if extension.eq_ignore_ascii_case("wav") {
        Some(InlineAssetMediaType::Wav)
    } else if extension.eq_ignore_ascii_case("ogg") {
        Some(InlineAssetMediaType::Ogg)
    } else if extension.eq_ignore_ascii_case("woff") {
        Some(InlineAssetMediaType::Woff)
    } else if extension.eq_ignore_ascii_case("woff2") {
        Some(InlineAssetMediaType::Woff2)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> LogicalAssetPath {
        LogicalAssetPath::parse(value).expect("fixture path must parse")
    }

    #[test]
    fn passive_media_types_have_one_case_insensitive_inline_policy() {
        let cases = [
            ("asset.png", "image/png"),
            ("asset.jpg", "image/jpeg"),
            ("asset.jpeg", "image/jpeg"),
            ("asset.gif", "image/gif"),
            ("asset.webp", "image/webp"),
            ("asset.avif", "image/avif"),
            ("asset.ico", "image/x-icon"),
            ("asset.mp4", "video/mp4"),
            ("asset.webm", "video/webm"),
            ("asset.mp3", "audio/mpeg"),
            ("asset.wav", "audio/wav"),
            ("asset.ogg", "audio/ogg"),
            ("asset.woff", "font/woff"),
            ("asset.WOFF2", "font/woff2"),
        ];

        for (name, content_type) in cases {
            let path = path(&format!("assets/{name}"));
            let delivery = AssetDelivery::for_authored(&path);
            assert_eq!(delivery.content_type(), content_type, "{name}");
            assert!(matches!(delivery, AssetDelivery::Inline(_)), "{name}");
        }
    }

    #[test]
    fn every_non_allowlisted_authored_file_is_an_inert_download() {
        for name in [
            "asset.svg",
            "asset.SVGZ",
            "asset.pdf",
            "asset.html",
            "asset.htm",
            "asset.xhtml",
            "asset.xml",
            "asset.js",
            "asset.mjs",
            "asset.css",
        ] {
            let path = path(&format!("assets/{name}"));
            assert_eq!(
                AssetDelivery::for_authored(&path),
                AssetDelivery::Attachment
            );
        }

        let opaque = path("assets/archive.bin");
        assert_eq!(
            AssetDelivery::for_authored(&opaque),
            AssetDelivery::Attachment
        );
    }

    #[test]
    fn generated_assets_are_inert_until_a_sanitizer_grants_capability() {
        assert_eq!(
            AssetDelivery::for_untrusted_generated(),
            AssetDelivery::Attachment
        );
    }
}
