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

/// A multiline string opener still active when a line begins. Only constructs
/// that genuinely span lines produce one: Python triple-quoted strings and
/// backtick literals (JavaScript template literals, Go raw strings). Rust raw
/// strings can also span lines but stay single-line here — a documented
/// approximation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultilineKind {
    TripleDouble,
    TripleSingle,
    Backtick,
}

/// Tokenize a single line. Pass `None` for `open` when no multiline string
/// carries in; offsets are char indices, matching the file viewer's
/// search-match columns. Always returns at least one token covering the whole
/// line (possibly a single `Normal`), so renderers can rely on full coverage
/// without a fallback path.
///
/// When the line may continue a multiline string opened on an earlier line,
/// pass the opener active at its first character (taken from
/// [`continuation_states`]) instead of `None`.
///
/// Adjacent plain-text characters (operators, punctuation, whitespace) are
/// coalesced into a single `Normal` token so a minified line does not produce
/// one token per character.
/// Direct-call fallback (tests, one-off lookups); the render path uses
/// [] through the bounded cache.
#[allow(dead_code)]
pub fn tokenize_continued(line: &str, lang: Language, open: Option<MultilineKind>) -> Vec<Token> {
    scan(line, lang, open, None).0
}

/// Tokenize only the `[0, end_exclusive)` char window of `line`.
///
/// The render path highlights at most one viewport of cells, so tokenizing a
/// 5 MiB single-line file to its end every frame would allocate millions of
/// tokens repeatedly. Windowed tokenization scans from the line start (the
/// `open` state makes the prefix load-bearing) but stops at `end_exclusive`,
/// bounding per-frame work to the visible width. The returned tokens cover
/// `[0, min(end_exclusive, len))` with the same classification the full scan
/// would produce on that prefix.
pub fn tokenize_window(
    line: &str,
    lang: Language,
    open: Option<MultilineKind>,
    end_exclusive: usize,
) -> Vec<Token> {
    scan(line, lang, open, Some(end_exclusive)).0
}

/// The multiline-string opener active at the start of each line, in order.
/// Prepared once per file load on the file-read worker and folded in via
/// `FileView::apply_prepared`, then threaded one entry per visible line into
/// [`tokenize_continued`]/[`tokenize_window`], so rendering stays O(visible
/// rows × visible width) while continuation lines keep their string color.
pub fn continuation_states(lines: &[String], lang: Language) -> Vec<Option<MultilineKind>> {
    let mut states = Vec::with_capacity(lines.len());
    let mut open = None;
    for line in lines {
        states.push(open);
        open = scan(line, lang, open, None).1;
    }
    states
}

fn scan(
    line: &str,
    lang: Language,
    open: Option<MultilineKind>,
    end_limit: Option<usize>,
) -> (Vec<Token>, Option<MultilineKind>) {
    // Windowed scans bound per-frame work to the viewport: collect at most
    // `limit + lookahead` chars instead of the whole line. The extra 64 chars
    // exist only so classification at the window edge (function-call parens,
    // char-literal shapes, identifier boundaries) agrees with the full scan;
    // token coverage stops at `target`.
    let (chars, target): (Vec<char>, usize) = match end_limit {
        None => {
            let chars: Vec<char> = line.chars().collect();
            let len = chars.len();
            (chars, len)
        }
        Some(limit) => {
            if limit == 0 {
                return (Vec::new(), open);
            }
            let take = limit.saturating_add(64);
            let chars: Vec<char> = line.chars().take(take).collect();
            let target = chars.len().min(limit);
            (chars, target)
        }
    };
    let len = chars.len();
    if target == 0 {
        // An empty line never opens or closes a multiline string.
        return (Vec::new(), open);
    }
    if lang == Language::Plain {
        return (
            vec![Token {
                start: 0,
                end: target,
                kind: Kind::Normal,
            }],
            None,
        );
    }
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut open = open;
    // Helper: clip a token end to the visible window. Full scans have
    // `target == len`, so this is a no-op there; windowed scans stop at the
    // viewport instead of tokenizing a multi-megabyte line to its end.
    let clip = |end: usize| end.min(target);
    // A line that begins inside a multiline string: emit string content up to
    // the closer (if any), then tokenize the remainder as fresh code.
    if let Some(kind) = open {
        if let Some(end) = find_multiline_close(&chars, 0, lang, kind) {
            let end = clip(end);
            tokens.push(Token {
                start: 0,
                end,
                kind: Kind::String,
            });
            i = end;
            open = None;
            if i >= target {
                return (tokens, open);
            }
        } else {
            tokens.push(Token {
                start: 0,
                end: target,
                kind: Kind::String,
            });
            return (tokens, open);
        }
    }
    while i < target {
        // Line comments (checked before anything else at this position).
        if starts_line_comment(&chars, i, lang) {
            tokens.push(Token {
                start: i,
                end: target,
                kind: Kind::Comment,
            });
            break;
        }
        // Same-line block comments: `/* … */` or `<!-- … -->`.
        if let Some(end) = block_comment_end(&chars, i, lang) {
            let end = clip(end);
            tokens.push(Token {
                start: i,
                end,
                kind: Kind::Comment,
            });
            i = end;
            continue;
        }
        let ch = chars[i];
        // Multiline openers: Python triple-quoted strings and backtick
        // literals. A same-line pair is one string token; an unclosed opener
        // strings the rest of the line and carries into the next one. Checked
        // before single-line strings so `"""` never splits into pieces.
        if let Some(kind) = multiline_opener_at(&chars, i, lang) {
            let body = i + opener_len(kind);
            if let Some(end) = find_multiline_close(&chars, body, lang, kind) {
                let end = clip(end);
                tokens.push(Token {
                    start: i,
                    end,
                    kind: Kind::String,
                });
                i = end;
                continue;
            }
            tokens.push(Token {
                start: i,
                end: target,
                kind: Kind::String,
            });
            return (tokens, Some(kind));
        }
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
            let end = if closed { clip(j) } else { target };
            tokens.push(Token {
                start: i,
                end,
                kind: Kind::String,
            });
            i = end;
            // An unclosed single-line string runs to the window end; the loop
            // exits (`i == target`) and `open` is unchanged (single-line
            // strings never carry). Windowed and full scans agree here.
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
                end: clip(j.max(i + 1)),
                kind: Kind::Number,
            });
            i = clip(j.max(i + 1));
            // A number split by the window edge keeps the prefix; the loop
            // exits when the window is covered.
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
            // Clip an identifier split by the window edge to its visible
            // prefix; classification uses the full word (plus lookahead for
            // the call paren), so the prefix keeps the right style.
            let end = clip(j);
            tokens.push(Token {
                start: i,
                end,
                kind,
            });
            i = end;
            continue;
        }
        // Anything else (operators, punctuation, whitespace): coalesce the
        // whole run into one `Normal` token instead of one token per char.
        // A minified JSON line is mostly such runs, so this cuts millions of
        // transient tokens down to thousands.
        {
            let mut j = i + 1;
            while j < target {
                // Stop where the next position would start something else.
                if starts_line_comment(&chars, j, lang) {
                    break;
                }
                if block_comment_end(&chars, j, lang).is_some() {
                    break;
                }
                if j < len && multiline_opener_at(&chars, j, lang).is_some() {
                    break;
                }
                let c = chars[j];
                if c.is_ascii_digit() {
                    break;
                }
                if c == '_' || c.is_alphabetic() {
                    break;
                }
                if c == '"' {
                    break;
                }
                if c == '`' && backtick_is_string(lang) {
                    break;
                }
                if c == '\'' && quote_is_string(&chars, j, lang) {
                    break;
                }
                j += 1;
            }
            tokens.push(Token {
                start: i,
                end: j,
                kind: Kind::Normal,
            });
            i = j;
        }
    }
    (tokens, open)
}

/// The opener width in chars: three for triple quotes, one for backticks.
fn opener_len(kind: MultilineKind) -> usize {
    match kind {
        MultilineKind::TripleDouble | MultilineKind::TripleSingle => 3,
        MultilineKind::Backtick => 1,
    }
}

/// The multiline opener starting at char offset `i`, if any.
fn multiline_opener_at(chars: &[char], i: usize, lang: Language) -> Option<MultilineKind> {
    let ch = chars[i];
    if lang == Language::Python
        && ch == '"'
        && chars.get(i + 1) == Some(&'"')
        && chars.get(i + 2) == Some(&'"')
    {
        return Some(MultilineKind::TripleDouble);
    }
    if lang == Language::Python
        && ch == '\''
        && chars.get(i + 1) == Some(&'\'')
        && chars.get(i + 2) == Some(&'\'')
    {
        return Some(MultilineKind::TripleSingle);
    }
    if ch == '`' && backtick_is_string(lang) {
        return Some(MultilineKind::Backtick);
    }
    None
}

/// Offset just past the closer for `kind`, searching from char offset `from`.
/// Returns `None` when the line ends inside the string. Backslash escapes
/// apply everywhere except Go raw strings, which end at the next backtick.
fn find_multiline_close(
    chars: &[char],
    from: usize,
    lang: Language,
    kind: MultilineKind,
) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        match kind {
            MultilineKind::TripleDouble => {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if chars[j] == '"'
                    && chars.get(j + 1) == Some(&'"')
                    && chars.get(j + 2) == Some(&'"')
                {
                    return Some(j + 3);
                }
                j += 1;
            }
            MultilineKind::TripleSingle => {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if chars[j] == '\''
                    && chars.get(j + 1) == Some(&'\'')
                    && chars.get(j + 2) == Some(&'\'')
                {
                    return Some(j + 3);
                }
                j += 1;
            }
            MultilineKind::Backtick => {
                if lang != Language::Go && chars[j] == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if chars[j] == '`' {
                    return Some(j + 1);
                }
                j += 1;
            }
        }
    }
    None
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
        tokenize_continued(line, lang, None)
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
        assert_eq!(
            language_for_path(Path::new("init.lua")),
            Language::Generic,
            "Lua has no dedicated variant; SQL keywords must not leak into it"
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
            let tokens = tokenize_continued(line, lang, None);
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

    fn continued(lines: &[&str], lang: Language) -> Vec<Vec<(String, Kind)>> {
        let owned: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let states = continuation_states(&owned, lang);
        assert_eq!(states.len(), lines.len());
        lines
            .iter()
            .zip(states)
            .map(|(line, open)| {
                let chars: Vec<char> = line.chars().collect();
                tokenize_continued(line, lang, open)
                    .iter()
                    .map(|token| (chars[token.start..token.end].iter().collect(), token.kind))
                    .collect()
            })
            .collect()
    }

    /// The review repro: keywords on continuation lines of a triple-quoted
    /// string must not classify as keywords.
    #[test]
    fn python_triple_quoted_continuations_stay_strings() {
        let rows = continued(
            &[r#"doc = """start"#, "return of the thing", r#"end""""#],
            Language::Python,
        );
        assert!(rows[1].iter().all(|(_, kind)| *kind == Kind::String));
        assert!(
            !rows[1].iter().any(|(_, kind)| *kind == Kind::Keyword),
            "`return` inside a docstring is string content"
        );
        // The closer ends the string; code after it tokenizes normally.
        assert!(rows[2].iter().any(|(_, kind)| *kind == Kind::String));
    }

    #[test]
    fn same_line_triple_pair_does_not_carry() {
        let owned = vec![r#"x = """done""" + 1"#.to_string()];
        let states = continuation_states(&owned, Language::Python);
        assert_eq!(states, vec![None]);
        let rows = continued(&[r#"x = """done""" + 1"#], Language::Python);
        assert!(rows[0].iter().any(|(_, kind)| *kind == Kind::Number));
    }

    #[test]
    fn js_template_literals_span_lines_with_escapes() {
        let rows = continued(
            &["const t = `a\\`b", "return ${x} done", "end` + 1;"],
            Language::Js,
        );
        // The escaped backtick must not close the literal early.
        assert!(rows[1].iter().all(|(_, kind)| *kind == Kind::String));
        assert!(rows[2].iter().any(|(_, kind)| *kind == Kind::String));
        assert!(rows[2].iter().any(|(_, kind)| *kind == Kind::Number));
    }

    #[test]
    fn go_raw_strings_span_lines() {
        let rows = continued(
            &[r#"s := `first"#, "return second", "third`;"],
            Language::Go,
        );
        assert!(rows[1].iter().all(|(_, kind)| *kind == Kind::String));
        assert!(rows[2].iter().any(|(_, kind)| *kind == Kind::String));
    }

    #[test]
    fn backtick_in_a_comment_opens_nothing() {
        let owned = vec!["// use `ticks` here".to_string(), "return 1;".to_string()];
        let states = continuation_states(&owned, Language::Js);
        assert_eq!(states, vec![None, None]);
    }

    #[test]
    fn single_quoted_strings_do_not_carry() {
        let owned = vec!["'open".to_string(), "return".to_string()];
        let states = continuation_states(&owned, Language::Python);
        assert_eq!(states, vec![None, None]);
    }

    /// Adjacent plain-text runs (punctuation/whitespace) must coalesce: a
    /// minified line must not produce one token per character.
    #[test]
    fn adjacent_normal_text_coalesces() {
        let tokens = tokenize_continued("   ", Language::Rust, None);
        assert_eq!(tokens.len(), 1, "spaces are one Normal run, not per char");
        assert_eq!(tokens[0].kind, Kind::Normal);

        let tokens = tokenize_continued("{}}", Language::Json, None);
        assert_eq!(
            tokens.len(),
            1,
            "adjacent punctuation coalesces, not one token per brace"
        );

        // Strings/keywords still split the run.
        let tokens = tokenize_continued(r#"{"a":1}"#, Language::Json, None);
        assert!(
            tokens.len() < r#"{"a":1}"#.chars().count(),
            "coalesced tokens are fewer than chars (got {})",
            tokens.len()
        );
        assert!(tokens.iter().any(|t| t.kind == Kind::String));
        assert!(tokens.iter().any(|t| t.kind == Kind::Number));
    }

    /// Windowed tokenization covers exactly the visible prefix with the same
    /// classification the full scan produces there.
    #[test]
    fn windowed_prefix_matches_full_scan() {
        let line = r#"fn load(path: &Path) -> usize { // open"#;
        let full = tokenize_continued(line, Language::Rust, None);
        for end in [0usize, 1, 5, 10, 20, line.chars().count()] {
            let win = tokenize_window(line, Language::Rust, None, end);
            // Clip the full scan to the window for comparison.
            let mut expected = Vec::new();
            for t in &full {
                if t.start >= end {
                    break;
                }
                expected.push(Token {
                    start: t.start,
                    end: t.end.min(end),
                    kind: t.kind,
                });
                if t.end >= end {
                    break;
                }
            }
            assert_eq!(win, expected, "window end {end}");
        }
    }
}
