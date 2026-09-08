//! Dependency-free syntax highlighting for the read-only file viewer.
//!
//! A stateless per-line tokenizer: keywords, strings, numbers, comments,
//! function calls, and `Capitalized` types. It deliberately adds no crates —
//! the viewer stays fast, offline, and theme-aware (styles resolve through the
//! active [`crate::ui::theme::Theme`] at render time, so no reparse is needed
//! when the theme changes).
//!
//! Known approximation: multi-line block comments. An unclosed `/*` colors the
//! rest of its own line, but continuation lines render as code. Line comments,
//! same-line blocks, strings, and keywords — the common cases — are exact.

use std::path::Path;

/// The language guessed from a file path. Drives comment markers, string
/// delimiters, and the keyword table. `Plain` disables comment detection so
/// prose (Markdown headings, plain text) is never dimmed as code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    Js,
    Go,
    CLike,
    Shell,
    Toml,
    Yaml,
    Json,
    Css,
    Html,
    Sql,
    Generic,
    Plain,
}

/// Guess the highlighting language from a file name. Extension matching is
/// case-insensitive; a few well-known bare names (`Dockerfile`, `Makefile`)
/// are recognized too. Unknown files fall back to `Generic` (`//` comments),
/// never to `Plain`, so code still gets strings, numbers, and keywords.
pub fn language_for_path(path: &Path) -> Language {
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        let lower = name.to_ascii_lowercase();
        if lower == "dockerfile" || lower == "containerfile" {
            return Language::Shell;
        }
        if lower == "makefile" || lower == "cmakelists.txt" {
            return Language::Shell;
        }
        if lower == "cargo.toml" || lower.ends_with(".toml") {
            return Language::Toml;
        }
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("rs") => Language::Rust,
        Some("py" | "pyi" | "pyw") => Language::Python,
        Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx") => Language::Js,
        Some("go") => Language::Go,
        Some(
            "java" | "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "cs" | "swift" | "kt" | "kts"
            | "scala",
        ) => Language::CLike,
        Some("sh" | "bash" | "zsh" | "fish" | "env") => Language::Shell,
        Some("toml" | "ini" | "cfg") => Language::Toml,
        Some("yaml" | "yml") => Language::Yaml,
        Some("json" | "jsonc" | "json5") => Language::Json,
        Some("css" | "scss" | "less") => Language::Css,
        Some("html" | "htm" | "xml" | "svg" | "vue" | "svelte") => Language::Html,
        Some("sql") => Language::Sql,
        Some("lua") => Language::Sql,
        Some("md" | "markdown" | "txt" | "text" | "log") => Language::Plain,
        _ => Language::Generic,
    }
}

/// The highlight role of one token. The renderer maps these through the
/// active theme; this module never names a color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Normal,
    Keyword,
    String,
    Comment,
    Number,
    Function,
    Type,
}

/// One highlighted token as half-open char indices into its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// Tokenize a single line. Offsets are char indices, matching the file
/// viewer's search-match columns. Always returns at least one token covering
/// the whole line (possibly a single `Normal`), so renderers can rely on full
/// coverage without a fallback path.
pub fn tokenize(line: &str, lang: Language) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    if len == 0 {
        return Vec::new();
    }
    if lang == Language::Plain {
        return vec![Token {
            start: 0,
            end: len,
            kind: Kind::Normal,
        }];
    }
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < len {
        // Line comments (checked before anything else at this position).
        if starts_line_comment(&chars, i, lang) {
            tokens.push(Token {
                start: i,
                end: len,
                kind: Kind::Comment,
            });
            break;
        }
        // Same-line block comments: `/* … */` or `<!-- … -->`.
        if let Some(end) = block_comment_end(&chars, i, lang) {
            tokens.push(Token {
                start: i,
                end,
                kind: Kind::Comment,
            });
            i = end;
            continue;
        }
        let ch = chars[i];
        // Strings.
        if ch == '"'
            || ch == '`' && backtick_is_string(lang)
            || ch == '\'' && quote_is_string(&chars, i, lang)
        {
            let delimiter = ch;
            let mut j = i + 1;
            let mut closed = false;
            while j < len {
                // Go raw strings (backticks) have no escapes.
                if delimiter == '`' && lang == Language::Go {
                    if chars[j] == '`' {
                        closed = true;
                        j += 1;
                        break;
                    }
                    j += 1;
                    continue;
                }
                if chars[j] == '\\' && j + 1 < len {
                    j += 2;
                    continue;
                }
                if chars[j] == delimiter {
                    closed = true;
                    j += 1;
                    break;
                }
                j += 1;
            }
            tokens.push(Token {
                start: i,
                end: if closed { j } else { len },
                kind: Kind::String,
            });
            i = if closed { j } else { len };
            continue;
        }
        // Numbers: hex/binary/octal prefixes, decimals, floats, type suffixes.
        if ch.is_ascii_digit() {
            let mut j = i;
            if ch == '0' && j + 1 < len && matches!(chars[j + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O')
            {
                j += 2;
                while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
            } else {
                while j < len && (chars[j].is_ascii_digit() || chars[j] == '_') {
                    j += 1;
                }
                if j < len && chars[j] == '.' && j + 1 < len && chars[j + 1].is_ascii_digit() {
                    j += 1;
                    while j < len && (chars[j].is_ascii_digit() || chars[j] == '_') {
                        j += 1;
                    }
                }
                if j < len && matches!(chars[j], 'e' | 'E') {
                    let mut k = j + 1;
                    if k < len && matches!(chars[k], '+' | '-') {
                        k += 1;
                    }
                    if k < len && chars[k].is_ascii_digit() {
                        k += 1;
                        while k < len && (chars[k].is_ascii_digit() || chars[k] == '_') {
                            k += 1;
                        }
                        j = k;
                    }
                }
                // Type suffixes (`u32`, `f64`, `i128`): letters and digits both
                // belong to the literal; `1..2` ranges still split because the
                // float arm above requires a digit after the dot.
                while j < len && chars[j].is_ascii_alphanumeric() {
                    j += 1;
                }
            }
            tokens.push(Token {
                start: i,
                end: j.max(i + 1),
                kind: Kind::Number,
            });
            i = j.max(i + 1);
            continue;
        }
        // Identifiers and keywords.
        if ch == '_' || ch.is_alphabetic() {
            let mut j = i + 1;
            while j < len && (chars[j] == '_' || chars[j].is_alphanumeric()) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let kind = if is_keyword(&word, lang) {
                Kind::Keyword
            } else if is_function_call(&chars, j) {
                Kind::Function
            } else if word
                .chars()
                .next()
                .is_some_and(|first| first.is_uppercase())
            {
                Kind::Type
            } else {
                Kind::Normal
            };
            tokens.push(Token {
                start: i,
                end: j,
                kind,
            });
            i = j;
            continue;
        }
        // Anything else (operators, punctuation, whitespace): one char.
        tokens.push(Token {
            start: i,
            end: i + 1,
            kind: Kind::Normal,
        });
        i += 1;
    }
    tokens
}

/// Does a line comment start at char offset `i`?
fn starts_line_comment(chars: &[char], i: usize, lang: Language) -> bool {
    let rest = &chars[i..];
    match lang {
        Language::Rust
        | Language::Js
        | Language::Go
        | Language::CLike
        | Language::Json
        | Language::Generic => rest.len() >= 2 && rest[0] == '/' && rest[1] == '/',
        Language::Python | Language::Toml | Language::Yaml => rest[0] == '#',
        Language::Shell => {
            if rest[0] != '#' {
                return false;
            }
            // In shells `#` only starts a comment at a word boundary; `a#b`
            // echoes literally.
            i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], ';' | '|' | '&' | '(')
        }
        Language::Css => false,
        Language::Html | Language::Plain => false,
        Language::Sql => {
            (rest.len() >= 2 && rest[0] == '-' && rest[1] == '-')
                || (rest.len() >= 2 && rest[0] == '/' && rest[1] == '/')
        }
    }
}

/// If a same-line block comment opens at `i`, return the char offset just past
/// its end (or the line end for an unclosed opener). Otherwise `None`.
fn block_comment_end(chars: &[char], i: usize, lang: Language) -> Option<usize> {
    let supports_c_block = matches!(
        lang,
        Language::Rust
            | Language::Js
            | Language::Go
            | Language::CLike
            | Language::Json
            | Language::Css
            | Language::Sql
            | Language::Generic
    );
    if supports_c_block && chars.len() >= i + 2 && chars[i] == '/' && chars[i + 1] == '*' {
        let mut j = i + 2;
        while j + 1 < chars.len() {
            if chars[j] == '*' && chars[j + 1] == '/' {
                return Some(j + 2);
            }
            j += 1;
        }
        return Some(chars.len());
    }
    if lang == Language::Html
        && chars.len() >= i + 4
        && chars[i] == '<'
        && chars[i + 1] == '!'
        && chars[i + 2] == '-'
        && chars[i + 3] == '-'
    {
        let mut j = i + 4;
        while j + 2 < chars.len() {
            if chars[j] == '-' && chars[j + 1] == '-' && chars[j + 2] == '>' {
                return Some(j + 3);
            }
            j += 1;
        }
        return Some(chars.len());
    }
    None
}

/// Is backtick a string delimiter in this language (JS templates, Go raw
/// strings)? Rust uses backticks nowhere; Markdown never reaches this module.
fn backtick_is_string(lang: Language) -> bool {
    matches!(lang, Language::Js | Language::Go | Language::Generic)
}

/// Is the `'` at `i` a string delimiter? Single-quote strings are normal in
/// Python, JS, shells, TOML, and SQL. In C-like languages a lone `'` is far
/// more likely a lifetime or punctuation, so only `'x'`-shaped char literals
/// count there.
fn quote_is_string(chars: &[char], i: usize, lang: Language) -> bool {
    match lang {
        Language::Python
        | Language::Js
        | Language::Shell
        | Language::Toml
        | Language::Yaml
        | Language::Json
        | Language::Sql => true,
        _ => {
            // `'c'` or `'\..'` char literal.
            (i + 2 < chars.len() && chars[i + 2] == '\'')
                || (i + 3 < chars.len() && chars[i + 1] == '\\' && chars[i + 3] == '\'')
        }
    }
}

/// Is the identifier immediately (modulo whitespace) followed by `(`?
/// Keywords are classified before this runs, so `if (` stays a keyword.
fn is_function_call(chars: &[char], end: usize) -> bool {
    let mut j = end;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    j < chars.len() && chars[j] == '('
}

fn is_keyword(word: &str, lang: Language) -> bool {
    if lang == Language::Sql {
        let lower = word.to_ascii_lowercase();
        return SQL_KEYWORDS.contains(&lower.as_str());
    }
    match lang {
        Language::Rust => RUST_KEYWORDS.contains(&word),
        Language::Python => PYTHON_KEYWORDS.contains(&word),
        Language::Js => JS_KEYWORDS.contains(&word),
        Language::Go => GO_KEYWORDS.contains(&word),
        Language::CLike => JS_KEYWORDS.contains(&word) || C_LIKE_EXTRA.contains(&word),
        Language::Shell => SHELL_KEYWORDS.contains(&word),
        Language::Toml | Language::Yaml | Language::Json => {
            matches!(word, "true" | "false" | "null" | "True" | "False" | "None")
        }
        Language::Css => matches!(
            word,
            "important" | "inherit" | "initial" | "unset" | "none" | "auto" | "true" | "false"
        ),
        Language::Html | Language::Generic => GENERIC_KEYWORDS.contains(&word),
        Language::Sql | Language::Plain => false,
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "yield",
    "do",
    "try",
    "union",
    "macro_rules",
    "None",
    "Some",
    "Ok",
    "Err",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

const JS_KEYWORDS: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
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
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "async",
    "of",
    "from",
    "as",
    "static",
    "get",
    "set",
    "package",
    "private",
    "protected",
    "public",
];

const C_LIKE_EXTRA: &[&str] = &[
    "int",
    "long",
    "short",
    "byte",
    "float",
    "double",
    "char",
    "boolean",
    "bool",
    "string",
    "void",
    "sizeof",
    "typedef",
    "signed",
    "unsigned",
    "namespace",
    "using",
    "template",
    "typename",
    "operator",
    "virtual",
    "override",
    "final",
    "abstract",
    "synchronized",
    "volatile",
    "transient",
    "throws",
    "extends",
    "sealed",
    "record",
    "sizeof",
];

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
    "true",
    "false",
    "nil",
    "iota",
];

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "return", "exit", "export", "local", "readonly", "declare",
    "trap", "true", "false",
];

const GENERIC_KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "return", "function", "class", "import", "true", "false", "null",
];

const SQL_KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "alter",
    "drop",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "on",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "distinct",
    "as",
    "null",
    "true",
    "false",
    "primary",
    "key",
    "foreign",
    "references",
    "index",
    "view",
    "union",
    "all",
    "exists",
    "in",
    "is",
    "like",
    "between",
    "case",
    "when",
    "then",
    "else",
    "end",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kinds(line: &str, lang: Language) -> Vec<(String, Kind)> {
        let chars: Vec<char> = line.chars().collect();
        tokenize(line, lang)
            .iter()
            .map(|token| (chars[token.start..token.end].iter().collect(), token.kind))
            .collect()
    }

    #[test]
    fn language_detection_covers_common_extensions() {
        assert_eq!(language_for_path(Path::new("main.rs")), Language::Rust);
        assert_eq!(language_for_path(Path::new("app.PY")), Language::Python);
        assert_eq!(language_for_path(Path::new("bundle.min.TSX")), Language::Js);
        assert_eq!(language_for_path(Path::new("Dockerfile")), Language::Shell);
        assert_eq!(language_for_path(Path::new("README.md")), Language::Plain);
        assert_eq!(language_for_path(Path::new("notes.txt")), Language::Plain);
        assert_eq!(
            language_for_path(Path::new("mystery.xyz")),
            Language::Generic
        );
    }

    #[test]
    fn rust_keywords_functions_and_types_classify() {
        let tokens = kinds("fn main() -> Option<String> {", Language::Rust);
        assert!(tokens.contains(&("fn".into(), Kind::Keyword)));
        assert!(tokens.contains(&("main".into(), Kind::Function)));
        assert!(tokens.contains(&("Option".into(), Kind::Type)));
        assert!(tokens.contains(&("String".into(), Kind::Type)));
    }

    #[test]
    fn rust_line_comment_and_string_win_over_keywords() {
        let tokens = kinds("let s = \"fn not_keyword\"; // let comment", Language::Rust);
        assert!(tokens.contains(&("let".into(), Kind::Keyword)));
        assert!(tokens
            .iter()
            .any(|(text, kind)| *kind == Kind::String && text.contains("fn")));
        let comment: String = tokens
            .iter()
            .filter(|(_, kind)| *kind == Kind::Comment)
            .map(|(text, _)| text.as_str())
            .collect();
        assert!(comment.contains("let comment"));
    }

    #[test]
    fn rust_lifetimes_are_not_strings() {
        let tokens = kinds("fn get(x: &'a str) {", Language::Rust);
        assert!(!tokens.iter().any(|(_, kind)| *kind == Kind::String));
        assert!(tokens.contains(&("fn".into(), Kind::Keyword)));
        assert!(tokens.contains(&("get".into(), Kind::Function)));
    }

    #[test]
    fn numbers_highlight_with_suffixes_and_hex() {
        let tokens = kinds("let x = 0xFF + 1_000u32 + 3.14;", Language::Rust);
        let numbers: Vec<_> = tokens
            .iter()
            .filter(|(_, kind)| *kind == Kind::Number)
            .map(|(text, _)| text.clone())
            .collect();
        assert!(numbers.iter().any(|text| text == "0xFF"));
        assert!(numbers.iter().any(|text| text == "1_000u32"));
        assert!(numbers.iter().any(|text| text == "3.14"));
    }

    #[test]
    fn python_hash_comments_but_markdown_stays_plain() {
        let tokens = kinds("# comment here", Language::Python);
        assert_eq!(tokens.last().map(|(_, kind)| kind), Some(&Kind::Comment));
        let plain = kinds("# Heading", Language::Plain);
        assert!(plain.iter().all(|(_, kind)| *kind == Kind::Normal));
    }

    #[test]
    fn sql_keywords_match_case_insensitively() {
        let tokens = kinds("SELECT id FROM users WHERE active;", Language::Sql);
        for word in ["SELECT", "FROM", "WHERE"] {
            assert!(
                tokens.contains(&(word.into(), Kind::Keyword)),
                "{word} should be a keyword"
            );
        }
    }

    #[test]
    fn same_line_block_comments_color() {
        let tokens = kinds("int x; /* hidden */ int y;", Language::CLike);
        assert!(tokens
            .iter()
            .any(|(text, kind)| *kind == Kind::Comment && text.contains("hidden")));
        assert!(tokens.contains(&("int".into(), Kind::Keyword)));
    }

    #[test]
    fn token_coverage_is_total_and_ordered() {
        for (line, lang) in [
            ("fn main() {", Language::Rust),
            ("", Language::Rust),
            ("   ", Language::Python),
            ("SELECT 1;", Language::Sql),
        ] {
            let len = line.chars().count();
            let tokens = tokenize(line, lang);
            if len == 0 {
                assert!(tokens.is_empty());
                continue;
            }
            assert_eq!(tokens.first().unwrap().start, 0);
            assert_eq!(tokens.last().unwrap().end, len);
            for pair in tokens.windows(2) {
                assert_eq!(pair[0].end, pair[1].start, "gap in {line:?}");
            }
        }
    }

    #[test]
    fn dockerfile_without_extension_highlights_shell_comments() {
        let dir = PathBuf::from("/tmp/proj");
        assert_eq!(language_for_path(&dir.join("Dockerfile")), Language::Shell);
        let tokens = kinds("RUN echo hi # done", Language::Shell);
        assert!(tokens.iter().any(|(_, kind)| *kind == Kind::Comment));
    }
}
