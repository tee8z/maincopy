//! Presentation-only token classes for fenced code. Highlighting never changes
//! an article's identity: the source splits into runs that concatenate back to
//! it exactly, and each classified run gets one application-owned class.

use super::code::CodeLanguage;

/// The closed set of colors a reader can see.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TokenKind {
    Comment,
    String,
    Number,
    Keyword,
    Type,
    Function,
    /// Attributes, macros, lifetimes, decorators, and section headers.
    Meta,
}

impl TokenKind {
    /// Returns only application-owned static class names.
    pub(super) const fn html_class(self) -> &'static str {
        match self {
            Self::Comment => "tok-comment",
            Self::String => "tok-string",
            Self::Number => "tok-number",
            Self::Keyword => "tok-keyword",
            Self::Type => "tok-type",
            Self::Function => "tok-function",
            Self::Meta => "tok-meta",
        }
    }
}

/// Lookahead bounds keep the scan linear on unbalanced input.
const MAX_ATTRIBUTE_BYTES: usize = 2048;
const MAX_CHARACTER_ESCAPE_BYTES: usize = 10;

/// One run of source. `None` is punctuation, whitespace, or a plain name.
pub(super) type Token<'source> = (Option<TokenKind>, &'source str);

struct BlockComment {
    open: &'static str,
    close: &'static str,
    nested: bool,
}

struct Quote {
    delimiter: &'static str,
    /// A backslash keeps the next character inside the string.
    escapes: bool,
    multiline: bool,
}

const fn quote(delimiter: &'static str, escapes: bool, multiline: bool) -> Quote {
    Quote {
        delimiter,
        escapes,
        multiline,
    }
}

struct Syntax {
    line_comments: &'static [&'static str],
    block_comment: Option<BlockComment>,
    /// Longer delimiters come first so `"""` wins over `"`.
    quotes: &'static [Quote],
    keywords: &'static [&'static str],
    keywords_ignore_case: bool,
    types: &'static [&'static str],
    /// A name directly before `(` is a function.
    calls: bool,
    /// A capitalized name with a lowercase letter is a type.
    capitalized_types: bool,
    /// `@name` is a decorator or at-rule.
    at_names: bool,
    /// A line that starts with `[` is a section header.
    section_headers: bool,
    /// Attributes, macros, lifetimes, character literals, and raw strings.
    rust: bool,
}

const PLAIN: Syntax = Syntax {
    line_comments: &[],
    block_comment: None,
    quotes: &[],
    keywords: &[],
    keywords_ignore_case: false,
    types: &[],
    calls: false,
    capitalized_types: false,
    at_names: false,
    section_headers: false,
    rust: false,
};

const C_BLOCK: Option<BlockComment> = Some(BlockComment {
    open: "/*",
    close: "*/",
    nested: false,
});

const RUST: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: Some(BlockComment {
        open: "/*",
        close: "*/",
        nested: true,
    }),
    quotes: &[quote("\"", true, true)],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ],
    types: &[
        "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32",
        "i64", "i128", "isize", "f32", "f64",
    ],
    calls: true,
    capitalized_types: true,
    rust: true,
    ..PLAIN
};

const JAVASCRIPT: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: &[
        quote("\"", true, false),
        quote("'", true, false),
        quote("`", true, true),
    ],
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "declare",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "namespace",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
    ],
    types: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    ],
    calls: true,
    capitalized_types: true,
    at_names: true,
    ..PLAIN
};

const PYTHON: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[
        quote("\"\"\"", true, true),
        quote("'''", true, true),
        quote("\"", true, false),
        quote("'", true, false),
    ],
    keywords: &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield",
    ],
    types: &[
        "bool", "bytes", "dict", "float", "int", "list", "set", "str", "tuple",
    ],
    calls: true,
    capitalized_types: true,
    at_names: true,
    ..PLAIN
};

const GO: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: &[
        quote("\"", true, false),
        quote("'", true, false),
        quote("`", false, true),
    ],
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "false",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "nil",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "true",
        "type",
        "var",
    ],
    types: &[
        "any", "bool", "byte", "error", "float32", "float64", "int", "int8", "int16", "int32",
        "int64", "rune", "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
    ],
    calls: true,
    capitalized_types: true,
    ..PLAIN
};

/// C, C++, C#, and Java share enough vocabulary for readable code.
const C_FAMILY: Syntax = Syntax {
    line_comments: &["//"],
    block_comment: C_BLOCK,
    quotes: &[quote("\"", true, false), quote("'", true, false)],
    keywords: &[
        "abstract",
        "auto",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "extends",
        "extern",
        "false",
        "final",
        "finally",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "inline",
        "interface",
        "namespace",
        "new",
        "null",
        "nullptr",
        "override",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "sizeof",
        "static",
        "struct",
        "super",
        "switch",
        "template",
        "this",
        "throw",
        "throws",
        "true",
        "try",
        "typedef",
        "union",
        "using",
        "virtual",
        "volatile",
        "while",
    ],
    types: &[
        "bool", "boolean", "byte", "char", "double", "float", "int", "long", "short", "signed",
        "string", "unsigned", "var", "void",
    ],
    calls: true,
    capitalized_types: true,
    at_names: true,
    ..PLAIN
};

const BASH: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[quote("\"", true, true), quote("'", false, true)],
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function",
        "if", "in", "local", "readonly", "return", "set", "shift", "source", "then", "unset",
        "until", "while",
    ],
    ..PLAIN
};

const SQL: Syntax = Syntax {
    line_comments: &["--"],
    block_comment: C_BLOCK,
    quotes: &[quote("'", false, true), quote("\"", false, false)],
    keywords: &[
        "add",
        "all",
        "alter",
        "and",
        "as",
        "begin",
        "by",
        "case",
        "check",
        "column",
        "commit",
        "create",
        "default",
        "delete",
        "desc",
        "distinct",
        "drop",
        "else",
        "end",
        "exists",
        "foreign",
        "from",
        "group",
        "having",
        "if",
        "in",
        "index",
        "inner",
        "insert",
        "into",
        "is",
        "join",
        "key",
        "left",
        "like",
        "limit",
        "not",
        "null",
        "offset",
        "on",
        "or",
        "order",
        "outer",
        "pragma",
        "primary",
        "references",
        "right",
        "rollback",
        "select",
        "set",
        "strict",
        "table",
        "then",
        "transaction",
        "union",
        "unique",
        "update",
        "values",
        "view",
        "when",
        "where",
        "with",
    ],
    keywords_ignore_case: true,
    calls: true,
    ..PLAIN
};

const TOML: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[
        quote("\"\"\"", true, true),
        quote("'''", false, true),
        quote("\"", true, false),
        quote("'", false, false),
    ],
    keywords: &["false", "true"],
    section_headers: true,
    ..PLAIN
};

const YAML: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[quote("\"", true, false)],
    keywords: &["false", "no", "null", "true", "yes"],
    ..PLAIN
};

const JSON: Syntax = Syntax {
    quotes: &[quote("\"", true, false)],
    keywords: &["false", "null", "true"],
    ..PLAIN
};

const NIX: Syntax = Syntax {
    line_comments: &["#"],
    block_comment: C_BLOCK,
    quotes: &[quote("\"", true, true)],
    keywords: &[
        "assert", "else", "false", "if", "import", "in", "inherit", "let", "null", "rec", "then",
        "true", "with",
    ],
    ..PLAIN
};

const RUBY: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[quote("\"", true, false), quote("'", true, false)],
    keywords: &[
        "begin", "case", "class", "def", "do", "else", "elsif", "end", "ensure", "false", "if",
        "module", "nil", "require", "rescue", "return", "self", "then", "true", "unless", "until",
        "when", "while", "yield",
    ],
    calls: true,
    capitalized_types: true,
    ..PLAIN
};

const DOCKERFILE: Syntax = Syntax {
    line_comments: &["#"],
    quotes: &[quote("\"", true, false)],
    keywords: &[
        "ADD",
        "ARG",
        "AS",
        "CMD",
        "COPY",
        "ENTRYPOINT",
        "ENV",
        "EXPOSE",
        "FROM",
        "HEALTHCHECK",
        "LABEL",
        "ONBUILD",
        "RUN",
        "SHELL",
        "STOPSIGNAL",
        "USER",
        "VOLUME",
        "WORKDIR",
    ],
    ..PLAIN
};

const CSS: Syntax = Syntax {
    block_comment: C_BLOCK,
    quotes: &[quote("\"", true, false), quote("'", true, false)],
    at_names: true,
    ..PLAIN
};

/// Markup and patches have no token grammar here; they stay uncolored.
const fn syntax(language: CodeLanguage) -> Option<&'static Syntax> {
    match language {
        CodeLanguage::Rust => Some(&RUST),
        CodeLanguage::JavaScript | CodeLanguage::TypeScript | CodeLanguage::Tsx => {
            Some(&JAVASCRIPT)
        }
        CodeLanguage::Python => Some(&PYTHON),
        CodeLanguage::Go => Some(&GO),
        CodeLanguage::C | CodeLanguage::Cpp | CodeLanguage::CSharp | CodeLanguage::Java => {
            Some(&C_FAMILY)
        }
        CodeLanguage::Bash => Some(&BASH),
        CodeLanguage::Sql => Some(&SQL),
        CodeLanguage::Toml => Some(&TOML),
        CodeLanguage::Yaml => Some(&YAML),
        CodeLanguage::Json => Some(&JSON),
        CodeLanguage::Nix => Some(&NIX),
        CodeLanguage::Ruby => Some(&RUBY),
        CodeLanguage::Dockerfile => Some(&DOCKERFILE),
        CodeLanguage::Css => Some(&CSS),
        CodeLanguage::Diff | CodeLanguage::Html | CodeLanguage::Xml => None,
    }
}

/// Split `source` into runs that concatenate back to it exactly. The scan is
/// one forward pass; every token starts at an ASCII byte or the first byte of
/// a character and consumes whole characters, so each run is valid UTF-8.
pub(super) fn tokens(language: CodeLanguage, source: &str) -> Vec<Token<'_>> {
    let Some(syntax) = syntax(language) else {
        return vec![(None, source)];
    };
    let mut scanner = Scanner {
        syntax,
        source,
        bytes: source.as_bytes(),
        position: 0,
        plain_start: 0,
        tokens: Vec::new(),
    };
    scanner.scan();
    scanner.tokens
}

struct Scanner<'source> {
    syntax: &'static Syntax,
    source: &'source str,
    bytes: &'source [u8],
    position: usize,
    plain_start: usize,
    tokens: Vec<Token<'source>>,
}

impl<'source> Scanner<'source> {
    fn scan(&mut self) {
        while self.position < self.bytes.len() {
            match self.token_at(self.position) {
                Some((kind, end)) => self.emit(kind, end),
                None => self.position = self.name_end(self.position).max(self.position + 1),
            }
        }
        self.flush_plain(self.bytes.len());
    }

    fn flush_plain(&mut self, end: usize) {
        if self.plain_start < end {
            self.tokens
                .push((None, &self.source[self.plain_start..end]));
        }
        self.plain_start = end;
    }

    fn emit(&mut self, kind: TokenKind, end: usize) {
        self.flush_plain(self.position);
        self.tokens
            .push((Some(kind), &self.source[self.position..end]));
        self.position = end;
        self.plain_start = end;
    }

    /// The classified token that starts at `start`, with its exclusive end.
    fn token_at(&self, start: usize) -> Option<(TokenKind, usize)> {
        let rest = &self.source[start..];
        let byte = self.bytes[start];
        if let Some(end) = self.comment_end(start, rest) {
            return Some((TokenKind::Comment, end));
        }
        if self.syntax.rust
            && let Some(token) = self.rust_token(start, rest)
        {
            return Some(token);
        }
        if let Some(end) = self.string_end(start, rest) {
            return Some((TokenKind::String, end));
        }
        if self.syntax.section_headers
            && byte == b'['
            && self.at_line_start(start)
            && let Some(line) = rest.lines().next()
            && line.trim_end().ends_with(']')
        {
            return Some((TokenKind::Meta, start + line.trim_end().len()));
        }
        if self.syntax.at_names && byte == b'@' {
            let end = self.name_end(start + 1);
            return (end > start + 1).then_some((TokenKind::Meta, end));
        }
        if byte.is_ascii_digit() {
            return Some((TokenKind::Number, self.number_end(start)));
        }
        let end = self.name_end(start);
        if end == start {
            return None;
        }
        self.name_kind(start, end).map(|kind| (kind, end))
    }

    fn at_line_start(&self, start: usize) -> bool {
        self.source[..start]
            .rsplit('\n')
            .next()
            .is_some_and(|line| line.trim().is_empty())
    }

    fn comment_end(&self, start: usize, rest: &str) -> Option<usize> {
        for marker in self.syntax.line_comments {
            // `#` also appears inside words such as `$#` and `a#b`.
            let word_start =
                *marker != "#" || start == 0 || self.bytes[start - 1].is_ascii_whitespace();
            if rest.starts_with(marker) && word_start {
                return Some(start + rest.find('\n').unwrap_or(rest.len()));
            }
        }
        let block = self.syntax.block_comment.as_ref()?;
        if !rest.starts_with(block.open) {
            return None;
        }
        let mut depth = 1_usize;
        let mut position = start + block.open.len();
        while position < self.bytes.len() {
            let rest = &self.source[position..];
            if rest.starts_with(block.close) {
                position += block.close.len();
                depth -= 1;
                if depth == 0 {
                    return Some(position);
                }
            } else if block.nested && rest.starts_with(block.open) {
                position += block.open.len();
                depth += 1;
            } else {
                position += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
        // An unterminated comment runs to the end, as a compiler would read it.
        Some(self.bytes.len())
    }

    fn string_end(&self, start: usize, rest: &str) -> Option<usize> {
        let quote = self
            .syntax
            .quotes
            .iter()
            .find(|quote| rest.starts_with(quote.delimiter))?;
        Some(self.quoted_end(start + quote.delimiter.len(), quote))
    }

    /// The end of a string whose opening delimiter ends at `position`.
    fn quoted_end(&self, mut position: usize, quote: &Quote) -> usize {
        while position < self.bytes.len() {
            let rest = &self.source[position..];
            if rest.starts_with(quote.delimiter) {
                return position + quote.delimiter.len();
            }
            let character = rest.chars().next().map_or(1, char::len_utf8);
            match self.bytes[position] {
                b'\n' if !quote.multiline => return position,
                b'\\' if quote.escapes => {
                    let escaped = rest[1..].chars().next().map_or(0, char::len_utf8);
                    position += 1 + escaped;
                }
                _ => position += character,
            }
        }
        self.bytes.len()
    }

    fn rust_token(&self, start: usize, rest: &str) -> Option<(TokenKind, usize)> {
        if rest.starts_with("#[") || rest.starts_with("#![") {
            return self
                .bracket_end(start + rest.find('[')?)
                .map(|end| (TokenKind::Meta, end));
        }
        if let Some(end) = self.rust_raw_string_end(start, rest) {
            return Some((TokenKind::String, end));
        }
        if !rest.starts_with('\'') {
            return None;
        }
        // `'a'` and `'\n'` are characters; `'a` alone is a lifetime.
        let mut characters = rest[1..].char_indices();
        let (_, first) = characters.next()?;
        if first == '\\' {
            // The longest escape is `\u{10FFFF}`.
            let escaped = rest[2..].chars().next()?.len_utf8();
            let close = rest[2 + escaped..]
                .bytes()
                .take(MAX_CHARACTER_ESCAPE_BYTES)
                .position(|byte| byte == b'\'')?;
            return Some((TokenKind::String, start + 2 + escaped + close + 1));
        }
        if let Some((second, '\'')) = characters.next() {
            return Some((TokenKind::String, start + 1 + second + 1));
        }
        let end = self.name_end(start + 1);
        (end > start + 1).then_some((TokenKind::Meta, end))
    }

    /// `r"…"`, `r#"…"#`, and their `b` and `c` prefixed forms.
    fn rust_raw_string_end(&self, start: usize, rest: &str) -> Option<usize> {
        if start > 0 && is_name_byte(self.bytes[start - 1]) {
            return None;
        }
        let prefix = ["br", "cr", "r"]
            .into_iter()
            .find(|prefix| rest.starts_with(prefix))?;
        let after = &rest[prefix.len()..];
        let hashes = after.bytes().take_while(|byte| *byte == b'#').count();
        if !after[hashes..].starts_with('"') {
            return None;
        }
        let body = start + prefix.len() + hashes + 1;
        let close = format!("\"{}", "#".repeat(hashes));
        Some(
            self.source[body..]
                .find(&close)
                .map_or(self.bytes.len(), |found| body + found + close.len()),
        )
    }

    /// The end of the bracket group that opens at `open`, when it closes
    /// within the attribute bound.
    fn bracket_end(&self, open: usize) -> Option<usize> {
        let mut depth = 0_usize;
        for (offset, byte) in self.bytes[open..]
            .iter()
            .take(MAX_ATTRIBUTE_BYTES)
            .enumerate()
        {
            match byte {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + offset + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn number_end(&self, start: usize) -> usize {
        let mut position = start;
        while position < self.bytes.len() {
            let byte = self.bytes[position];
            let fraction =
                byte == b'.' && self.bytes.get(position + 1).is_some_and(u8::is_ascii_digit);
            if byte.is_ascii_alphanumeric() || byte == b'_' || fraction {
                position += 1;
            } else {
                break;
            }
        }
        position
    }

    /// The end of the name that starts at `start`, or `start` when none does.
    fn name_end(&self, start: usize) -> usize {
        let mut position = start;
        while position < self.bytes.len() && is_name_byte(self.bytes[position]) {
            position += 1;
        }
        position
    }

    fn name_kind(&self, start: usize, end: usize) -> Option<TokenKind> {
        let syntax = self.syntax;
        let name = &self.source[start..end];
        let keyword = syntax.keywords.iter().any(|keyword| {
            if syntax.keywords_ignore_case {
                keyword.eq_ignore_ascii_case(name)
            } else {
                *keyword == name
            }
        });
        if keyword {
            return Some(TokenKind::Keyword);
        }
        if syntax.types.contains(&name) {
            return Some(TokenKind::Type);
        }
        let next = self.bytes.get(end).copied();
        if syntax.rust && next == Some(b'!') && self.bytes.get(end + 1) != Some(&b'=') {
            return Some(TokenKind::Meta);
        }
        if syntax.capitalized_types
            && name.starts_with(|first: char| first.is_ascii_uppercase())
            && name.bytes().any(|byte| byte.is_ascii_lowercase())
        {
            return Some(TokenKind::Type);
        }
        // `fn name<T>(…)` declares a function even though `<` follows the name.
        let declared = syntax.rust
            && self.source[..start]
                .trim_end()
                .strip_suffix("fn")
                .is_some_and(|before| !before.bytes().next_back().is_some_and(is_name_byte));
        (syntax.calls && (next == Some(b'(') || declared)).then_some(TokenKind::Function)
    }
}

/// Names are ASCII words; any non-ASCII character continues one, which keeps
/// every boundary on a whole character.
const fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classified(language: CodeLanguage, source: &str) -> Vec<(TokenKind, &str)> {
        let tokens = tokens(language, source);
        let rebuilt: String = tokens.iter().map(|(_, text)| *text).collect();
        assert_eq!(rebuilt, source, "runs must concatenate back to the source");
        tokens
            .into_iter()
            .filter_map(|(kind, text)| kind.map(|kind| (kind, text)))
            .collect()
    }

    #[test]
    fn rust_items_get_their_expected_classes() {
        use TokenKind::{Comment, Function, Keyword, Meta, Number, String, Type};
        let source = "#[derive(Debug)]\n// note\npub fn area<'a>(shape: &'a Shape) -> f64 {\n    let label = format!(\"{}\", 'x');\n    Vec::new();\n    1_000.5 * shape.side /* nested /* ok */ */\n}\n";
        assert_eq!(
            classified(CodeLanguage::Rust, source),
            [
                (Meta, "#[derive(Debug)]"),
                (Comment, "// note"),
                (Keyword, "pub"),
                (Keyword, "fn"),
                (Function, "area"),
                (Meta, "'a"),
                (Meta, "'a"),
                (Type, "Shape"),
                (Type, "f64"),
                (Keyword, "let"),
                (Meta, "format"),
                (String, "\"{}\""),
                (String, "'x'"),
                (Type, "Vec"),
                (Function, "new"),
                (Number, "1_000.5"),
                (Comment, "/* nested /* ok */ */"),
            ]
        );
    }

    #[test]
    fn rust_strings_keep_their_contents_uncolored_inside() {
        use TokenKind::{Keyword, Number, String};
        let source = "let a = r#\"raw \"quoted\" // not a comment\"#; let b = \"esc \\\" fn\"; let c = b'\\n'; let d = 0..10;";
        assert_eq!(
            classified(CodeLanguage::Rust, source),
            [
                (Keyword, "let"),
                (String, "r#\"raw \"quoted\" // not a comment\"#"),
                (Keyword, "let"),
                (String, "\"esc \\\" fn\""),
                (Keyword, "let"),
                (String, "'\\n'"),
                (Keyword, "let"),
                (Number, "0"),
                (Number, "10"),
            ]
        );
    }

    #[test]
    fn constants_comparisons_and_plain_names_stay_plain() {
        assert_eq!(
            classified(CodeLanguage::Rust, "MAX_SIZE != limit; value.field"),
            []
        );
    }

    #[test]
    fn other_languages_use_their_own_comments_strings_and_keywords() {
        use TokenKind::{Comment, Function, Keyword, Meta, Number, String, Type};
        assert_eq!(
            classified(
                CodeLanguage::Toml,
                "[package]\nname = \"demo\" # label\nedition = 2024\nok = true\n"
            ),
            [
                (Meta, "[package]"),
                (String, "\"demo\""),
                (Comment, "# label"),
                (Number, "2024"),
                (Keyword, "true"),
            ]
        );
        assert_eq!(
            classified(
                CodeLanguage::Bash,
                "if [ $# -gt 0 ]; then echo 'it''s' \"$HOME\"; fi # done\n"
            ),
            [
                (Keyword, "if"),
                (Number, "0"),
                (Keyword, "then"),
                (String, "'it'"),
                (String, "'s'"),
                (String, "\"$HOME\""),
                (Keyword, "fi"),
                (Comment, "# done"),
            ]
        );
        assert_eq!(
            classified(
                CodeLanguage::JavaScript,
                "const el = document.querySelector(`#${id}`); // find\nnew Map()"
            ),
            [
                (Keyword, "const"),
                (Function, "querySelector"),
                (String, "`#${id}`"),
                (Comment, "// find"),
                (Keyword, "new"),
                (Type, "Map"),
            ]
        );
        assert_eq!(
            classified(
                CodeLanguage::Css,
                "@media print { a { margin: 1.5rem } } /* x */"
            ),
            [(Meta, "@media"), (Number, "1.5rem"), (Comment, "/* x */")]
        );
        assert_eq!(
            classified(CodeLanguage::Sql, "SELECT count(*) FROM posts -- all"),
            [
                (Keyword, "SELECT"),
                (Function, "count"),
                (Keyword, "FROM"),
                (Comment, "-- all"),
            ]
        );
    }

    #[test]
    fn unterminated_and_non_ascii_input_is_split_on_character_boundaries() {
        for (language, source) in [
            (CodeLanguage::Rust, "let s = \"héllo → wörld"),
            (CodeLanguage::Rust, "/* never closed é"),
            (CodeLanguage::Rust, "let ñame = 'é'; let 'lifetime; #[oops"),
            (CodeLanguage::JavaScript, "const s = 'line ends\nnext';"),
            (CodeLanguage::Python, "x = '''multi\nline''' # ok ✓"),
            (CodeLanguage::Json, "{\"clé\": \"\\u00e9\\"),
            (CodeLanguage::Rust, "r#\"open raw é"),
            (CodeLanguage::Rust, "'"),
            (CodeLanguage::Rust, "'\\"),
            (CodeLanguage::Toml, "[unclosed\n"),
        ] {
            classified(language, source);
        }
    }

    #[test]
    fn markup_and_patches_are_one_plain_run() {
        for language in [CodeLanguage::Html, CodeLanguage::Xml, CodeLanguage::Diff] {
            assert_eq!(tokens(language, "<a> + b"), [(None, "<a> + b")]);
        }
    }
}
