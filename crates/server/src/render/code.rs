//! Safe author-declared language metadata for fenced code blocks.

const LANGUAGE_ALIASES: &[(&str, CodeLanguage)] = &[
    ("bash", CodeLanguage::Bash),
    ("sh", CodeLanguage::Bash),
    ("shell", CodeLanguage::Bash),
    ("c", CodeLanguage::C),
    ("cpp", CodeLanguage::Cpp),
    ("c++", CodeLanguage::Cpp),
    ("csharp", CodeLanguage::CSharp),
    ("cs", CodeLanguage::CSharp),
    ("css", CodeLanguage::Css),
    ("diff", CodeLanguage::Diff),
    ("patch", CodeLanguage::Diff),
    ("dockerfile", CodeLanguage::Dockerfile),
    ("go", CodeLanguage::Go),
    ("html", CodeLanguage::Html),
    ("java", CodeLanguage::Java),
    ("javascript", CodeLanguage::JavaScript),
    ("js", CodeLanguage::JavaScript),
    ("json", CodeLanguage::Json),
    ("nix", CodeLanguage::Nix),
    ("python", CodeLanguage::Python),
    ("py", CodeLanguage::Python),
    ("ruby", CodeLanguage::Ruby),
    ("rb", CodeLanguage::Ruby),
    ("rust", CodeLanguage::Rust),
    ("rs", CodeLanguage::Rust),
    ("sql", CodeLanguage::Sql),
    ("toml", CodeLanguage::Toml),
    ("typescript", CodeLanguage::TypeScript),
    ("ts", CodeLanguage::TypeScript),
    ("tsx", CodeLanguage::Tsx),
    ("xml", CodeLanguage::Xml),
    ("yaml", CodeLanguage::Yaml),
    ("yml", CodeLanguage::Yaml),
];

/// A closed set prevents authored fence text from becoming active HTML.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodeLanguage {
    Bash,
    C,
    Cpp,
    CSharp,
    Css,
    Diff,
    Dockerfile,
    Go,
    Html,
    Java,
    JavaScript,
    Json,
    Nix,
    Python,
    Ruby,
    Rust,
    Sql,
    Toml,
    TypeScript,
    Tsx,
    Xml,
    Yaml,
}

impl CodeLanguage {
    /// Recognizes one complete fence-info value without guessing from source.
    pub(super) fn from_fence_info(info: &str) -> Option<Self> {
        if !info.is_ascii() {
            return None;
        }
        LANGUAGE_ALIASES
            .iter()
            .find_map(|(alias, language)| info.eq_ignore_ascii_case(alias).then_some(*language))
    }

    /// Returns only application-owned static class names.
    pub(super) const fn html_class(self) -> &'static str {
        match self {
            Self::Bash => "language-bash",
            Self::C => "language-c",
            Self::Cpp => "language-cpp",
            Self::CSharp => "language-csharp",
            Self::Css => "language-css",
            Self::Diff => "language-diff",
            Self::Dockerfile => "language-dockerfile",
            Self::Go => "language-go",
            Self::Html => "language-html",
            Self::Java => "language-java",
            Self::JavaScript => "language-javascript",
            Self::Json => "language-json",
            Self::Nix => "language-nix",
            Self::Python => "language-python",
            Self::Ruby => "language-ruby",
            Self::Rust => "language-rust",
            Self::Sql => "language-sql",
            Self::Toml => "language-toml",
            Self::TypeScript => "language-typescript",
            Self::Tsx => "language-tsx",
            Self::Xml => "language-xml",
            Self::Yaml => "language-yaml",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_ascii_aliases_map_to_static_canonical_classes() {
        for (alias, expected) in LANGUAGE_ALIASES {
            assert_eq!(CodeLanguage::from_fence_info(alias), Some(*expected));
            assert_eq!(
                CodeLanguage::from_fence_info(&alias.to_ascii_uppercase()),
                Some(*expected)
            );
            assert!(expected.html_class().starts_with("language-"));
            assert!(
                expected
                    .html_class()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
            );
        }
    }

    #[test]
    fn arbitrary_or_ambiguous_fence_text_never_becomes_a_class() {
        for rejected in [
            "",
            "text",
            "ascii",
            "mermaid",
            "rust linenos",
            " rust",
            "rust ",
            "rüst",
            "rust\" onclick=\"alert(1)",
        ] {
            assert_eq!(CodeLanguage::from_fence_info(rejected), None);
        }
    }
}
