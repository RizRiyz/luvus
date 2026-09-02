//! URL detection in a pane's grid (docs/58 — clickable links).
//!
//! Pure: it takes the rows a [`VtEngine`](crate::terminal::vt::VtEngine) renders
//! and returns what it found, so the whole thing is unit-testable without a PTY.
//!
//! Scanning is **cursor-local**, never whole-screen: [`link_at`] walks out from
//! one cell to the token's edges and stops. A pane full of text costs the same
//! as an empty one, which is what keeps this off the render hot path.

use std::path::PathBuf;

/// Decode an OSC 8 `file://` target into an absolute local path.
///
/// Only an empty authority or `localhost` is accepted. Invalid escaping,
/// control characters, query strings, fragments, and relative paths are
/// rejected so terminal output cannot hand an arbitrary URI to the OS.
pub fn file_uri_path(uri: &str) -> Option<PathBuf> {
    if uri
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return None;
    }
    let rest = uri.strip_prefix("file://")?;
    if rest.contains(['?', '#']) {
        return None;
    }
    let encoded_path = if rest.starts_with('/') {
        rest
    } else {
        let slash = rest.find('/')?;
        let authority = &rest[..slash];
        if !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        &rest[slash..]
    };
    let decoded = percent_decode_uri_path(encoded_path)?;
    if decoded.starts_with("//") || decoded.starts_with("\\\\") {
        return None;
    }

    #[cfg(windows)]
    let decoded = {
        let bytes = decoded.as_bytes();
        if bytes.len() >= 4
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
            && bytes[3] == b'/'
        {
            decoded[1..].to_string()
        } else {
            decoded
        }
    };

    let path = PathBuf::from(decoded);
    path.is_absolute().then_some(path)
}

fn percent_decode_uri_path(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = hex_value(*bytes.get(index + 1)?)?;
        let low = hex_value(*bytes.get(index + 2)?)?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.chars().any(char::is_control)).then_some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// What a grid token turned out to be.
///
/// A path is returned **unresolved**: this module does no IO, so whether the file
/// exists (and what it is relative to) is settled by the app, which knows the
/// pane's working directory.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    /// An `http`/`https` URL, ready to hand to the OS.
    Url(String),
    /// A scheme-less token. It may be a **path** (with a `:line[:col]` suffix, the
    /// shape compilers print) or a **bare domain** (with a `:port`, the shape a
    /// dev server prints) — and `main.rs` is legitimately both, since `.rs` is a
    /// real TLD. Which one it is depends on the filesystem, so the app decides.
    Path {
        /// The token exactly as written, suffix included. What [`as_domain`] reads.
        raw: String,
        /// The path with any `:line[:col]` split off.
        text: String,
        line: Option<u32>,
    },
}

/// Top-level domains a bare `host.tld` is offered for.
///
/// A list rather than "any alphabetic suffix", because without one every
/// `notes.txt` that is not on disk would offer to open a website. It covers the
/// gTLDs and country codes people actually link to; a domain under something
/// exotic still works if it is written with its `https://`.
const KNOWN_TLDS: &[&str] = &[
    "com", "org", "net", "edu", "gov", "mil", "int", "info", "biz", "name", "pro", "app", "dev",
    "io", "ai", "co", "me", "tv", "cc", "xyz", "site", "online", "store", "shop", "blog", "cloud",
    "tech", "space", "live", "life", "world", "today", "news", "media", "page", "link", "click",
    "run", "sh", "gg", "gl", "ly", "to", "so", "st", "is", "it", "in", "id", "ie", "il", "uk",
    "de", "fr", "es", "nl", "be", "ch", "at", "se", "no", "dk", "fi", "pl", "cz", "pt", "gr", "ru",
    "ua", "tr", "jp", "cn", "kr", "tw", "hk", "sg", "my", "th", "vn", "ph", "au", "nz", "ca", "mx",
    "br", "ar", "cl", "za", "ng", "ke", "eg", "ae", "sa", "eu", "us", "rs", "md", "lol",
];

/// A bare `host[:port][/path]` worth offering as a URL, e.g. `luvus.dev/docs` or
/// `localhost:3000`. Returns the text to build the URL from, or `None`.
///
/// Deliberately strict, because this is the one detector with no existence check
/// behind it:
///
/// - **all lower case** — domains are written that way, and it is what keeps
///   `README.md` and `Cargo.toml` from being read as Moldovan and Tongan sites;
/// - a **known TLD** (or bare `localhost` / `127.0.0.1`);
/// - well-formed labels, so no empty parts and no leading or trailing hyphens.
pub fn as_domain(raw: &str) -> Option<&str> {
    let (authority, _) = raw.split_once('/').unwrap_or((raw, ""));
    // A port is digits; anything else after a colon is not an authority we know.
    let (host, has_port) = match authority.split_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (h, true)
        }
        Some(_) => return None,
        None => (authority, false),
    };
    if host != host.to_ascii_lowercase() {
        return None;
    }
    // A local address only counts with a port on it: that is what makes it a dev
    // server rather than the word "localhost" sitting in a sentence.
    if host == "localhost" || host == "127.0.0.1" {
        return has_port.then_some(raw);
    }
    let mut labels = host.split('.').peekable();
    let mut last = "";
    let mut count = 0;
    while let Some(label) = labels.next() {
        if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        count += 1;
        if labels.peek().is_none() {
            last = label;
        }
    }
    (count >= 2 && KNOWN_TLDS.contains(&last)).then_some(raw)
}

/// The scheme to open a bare domain with: dev servers on `localhost` speak plain
/// `http`, everything else gets `https`.
pub fn domain_url(domain: &str) -> String {
    let local = domain.starts_with("localhost") || domain.starts_with("127.0.0.1");
    format!("{}://{domain}", if local { "http" } else { "https" })
}

/// Something clickable found in the grid, plus the cells it occupies so the
/// hovered one can be underlined. More than one span when it soft-wraps at the
/// right edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub hit: Hit,
    /// `(row, start_col, end_col)` in grid coordinates, `end_col` exclusive.
    pub spans: Vec<(u16, u16, u16)>,
}

impl Link {
    /// Does this link cover grid cell (`col`, `row`)?
    pub fn covers(&self, col: u16, row: u16) -> bool {
        self.spans
            .iter()
            .any(|(r, a, b)| *r == row && col >= *a && col < *b)
    }
}

/// How many rows either side of the cursor a wrapped URL may span. A URL longer
/// than a few screen widths is prose that happens to contain a scheme, and the
/// bound keeps a pathological grid from turning a hover into a long walk.
const MAX_WRAP: usize = 4;

/// Characters that may appear inside a URL or a path: RFC 3986 unreserved +
/// reserved plus `\` for Windows paths, **ASCII only**, so a space, a
/// box-drawing glyph or CJK text ends the token.
///
/// ASCII-only also means a token's byte offsets equal its char offsets, which
/// [`link_at`] relies on when it locates the scheme.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&\'()*+,;=%\\".contains(c)
}

/// Split a trailing `:line` or `:line:col` off a path token, the shape compilers
/// and test runners print (`src/main.rs:42:7`).
///
/// Never eats a Windows drive letter: `C:\src` has a one-character head, and the
/// tail after the colon is not all digits anyway.
fn split_line_suffix(s: &str) -> (&str, Option<u32>) {
    let (mut base, mut line) = (s, None);
    for _ in 0..2 {
        let Some((head, tail)) = base.rsplit_once(':') else {
            break;
        };
        if head.len() <= 1 || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        // Walking right to left, so the column is seen first and the line
        // overwrites it — which is what we want to jump to.
        line = tail.parse().ok();
        base = head;
    }
    (base, line)
}

/// Is `s` shaped like a path worth checking on disk?
///
/// Either it carries a separator, or its last component has a short extension.
/// Without this a bare word like `notes` would be stat-ed on every hover; with it
/// the filesystem is only asked about things that look like files.
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    let last = s.rsplit(['/', '\\']).next().unwrap_or(s);
    match last.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Does row `i` soft-wrap into `i + 1`? `visible_rows` pads short lines out with
/// spaces, so a filled last cell means the text ran on rather than ended.
fn wraps(rows: &[String], i: usize) -> bool {
    let filled = |s: &String, last: bool| {
        let mut it = s.chars();
        let c = if last { it.next_back() } else { it.next() };
        c.is_some_and(|c| !c.is_whitespace())
    };
    rows.get(i).is_some_and(|s| filled(s, true))
        && rows.get(i + 1).is_some_and(|s| filled(s, false))
}

/// The wrapped run of rows containing `row`, flattened to `(char, row, col)`.
///
/// Joining rows here is what lets a URL survive an 80-column pane: agents print
/// long links constantly, and a host terminal that only sees the final screen
/// can only ever offer you the half before the wrap.
fn logical_line(rows: &[String], row: u16) -> Vec<(char, u16, u16)> {
    let r = row as usize;
    if r >= rows.len() {
        return Vec::new();
    }
    let mut start = r;
    while start > 0 && r - start < MAX_WRAP && wraps(rows, start - 1) {
        start -= 1;
    }
    let mut end = r;
    while end + 1 < rows.len() && end - r < MAX_WRAP && wraps(rows, end) {
        end += 1;
    }
    let mut cells = Vec::new();
    for (i, line) in rows.iter().enumerate().take(end + 1).skip(start) {
        for (c, ch) in line.chars().enumerate() {
            cells.push((ch, i as u16, c as u16));
        }
    }
    cells
}

/// Count occurrences of `ch` in `cells[lo..hi]`.
fn count(cells: &[(char, u16, u16)], ch: char) -> usize {
    cells.iter().filter(|(c, _, _)| *c == ch).count()
}

/// The link under grid cell (`col`, `row`), if there is one.
///
/// A URL needs an `http://` or `https://` scheme: bare `www.`, `file://` and
/// custom schemes are deliberately not offered, because this text comes from an
/// agent and the click ends up at the OS handler. Anything else path-shaped comes
/// back as [`Hit::Path`] for the caller to resolve and check on disk.
pub fn link_at(rows: &[String], col: u16, row: u16) -> Option<Link> {
    let cells = logical_line(rows, row);
    let idx = cells.iter().position(|(_, r, c)| *r == row && *c == col)?;
    if !is_token_char(cells[idx].0) {
        return None;
    }

    // Widen to the whitespace-delimited token around the cursor.
    let mut lo = idx;
    while lo > 0 && is_token_char(cells[lo - 1].0) {
        lo -= 1;
    }
    let mut hi = idx + 1;
    while hi < cells.len() && is_token_char(cells[hi].0) {
        hi += 1;
    }

    // A URL starts at its scheme, so `(https://x` and `see:https://x` both resolve
    // to the bare URL rather than dragging their prefix along. Without a scheme the
    // token is a path candidate and keeps its full extent.
    let token: String = cells[lo..hi].iter().map(|(c, _, _)| *c).collect();
    let is_url = match token.find("https://").or_else(|| token.find("http://")) {
        Some(at) => {
            lo += at; // token is ASCII, so the byte offset is the cell offset
            true
        }
        None => false,
    };

    // Trim trailing marks that belong to the prose around it, not to it: sentence
    // punctuation, and closing brackets that were never opened inside.
    while hi > lo {
        let drop = match cells[hi - 1].0 {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => count(&cells[lo..hi], ')') > count(&cells[lo..hi], '('),
            ']' => count(&cells[lo..hi], ']') > count(&cells[lo..hi], '['),
            _ => false,
        };
        if !drop {
            break;
        }
        hi -= 1;
    }

    // Trimming can pull the target out from under the cursor ("https://x." with
    // the cursor on the dot is a hover over prose, not over the link).
    if idx < lo || idx >= hi {
        return None;
    }

    let text: String = cells[lo..hi].iter().map(|(c, _, _)| *c).collect();
    let hit = if is_url {
        // A scheme with no host is not a link.
        let host = text.split("://").nth(1)?;
        if host.is_empty() || host.starts_with('/') {
            return None;
        }
        Hit::Url(text)
    } else {
        // `src/main.rs:42:7` is one reference: the whole thing underlines, but the
        // path handed on excludes the position.
        let (base, line) = split_line_suffix(&text);
        // Either shape earns a hit; the app works out which it actually is.
        if !looks_like_path(base) && as_domain(&text).is_none() {
            return None;
        }
        Hit::Path {
            raw: text.clone(),
            text: base.to_string(),
            line,
        }
    };

    // Group the cells into one span per row.
    let mut spans: Vec<(u16, u16, u16)> = Vec::new();
    for (_, r, c) in &cells[lo..hi] {
        match spans.last_mut() {
            Some((sr, _, e)) if sr == r && *e == *c => *e = c + 1,
            _ => spans.push((*r, *c, *c + 1)),
        }
    }
    Some(Link { hit, spans })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_only_absolute_local_file_uris() {
        #[cfg(unix)]
        {
            assert_eq!(
                file_uri_path("file:///Users/example/My%20File.rs"),
                Some(PathBuf::from("/Users/example/My File.rs"))
            );
            assert_eq!(
                file_uri_path("file://localhost/Users/example/main.rs"),
                Some(PathBuf::from("/Users/example/main.rs"))
            );
        }
        #[cfg(windows)]
        assert_eq!(
            file_uri_path("file:///C:/Users/example/main.rs"),
            Some(PathBuf::from("C:/Users/example/main.rs"))
        );

        for rejected in [
            "file://server/share/main.rs",
            "file:////server/share/main.rs",
            "file://relative.rs",
            "file:///bad%2",
            "file:///bad%GG",
            "file:///bad%00name",
            "file:///path.rs?query",
            "file:///path.rs#fragment",
            "file:///literal space.rs",
            "vscode://file/path.rs",
            "https://example.com/path.rs",
        ] {
            assert_eq!(file_uri_path(rejected), None, "accepted {rejected:?}");
        }
    }

    /// Pad to a fixed width, the way `visible_rows` hands them over.
    fn grid(lines: &[&str], w: usize) -> Vec<String> {
        lines.iter().map(|l| format!("{l:<w$}")).collect()
    }

    fn url_at(lines: &[&str], w: usize, col: u16, row: u16) -> Option<String> {
        match link_at(&grid(lines, w), col, row)?.hit {
            Hit::Url(u) => Some(u),
            Hit::Path { .. } => None,
        }
    }

    fn path_at(lines: &[&str], w: usize, col: u16, row: u16) -> Option<(String, Option<u32>)> {
        match link_at(&grid(lines, w), col, row)?.hit {
            Hit::Path { text, line, .. } => Some((text, line)),
            Hit::Url(_) => None,
        }
    }

    #[test]
    fn finds_a_plain_url_anywhere_along_it() {
        let g = ["see https://luvus.dev/docs for more"];
        for col in 4..=25 {
            assert_eq!(
                url_at(&g, 40, col, 0).as_deref(),
                Some("https://luvus.dev/docs"),
                "col {col}"
            );
        }
        // Off the ends: the prose either side is not a link.
        assert_eq!(url_at(&g, 40, 2, 0), None);
        assert_eq!(url_at(&g, 40, 30, 0), None);
    }

    #[test]
    fn ignores_text_without_a_scheme() {
        // Bare hostnames and other schemes are deliberately not offered.
        for line in [
            "visit www.luvus.dev today",
            "open file:///etc/hosts now",
            "run javascript:alert(1) no",
            "just some ordinary words",
        ] {
            for col in 0..line.len() as u16 {
                assert_eq!(url_at(&[line], 40, col, 0), None, "{line:?} col {col}");
            }
        }
    }

    /// Prose punctuation right after a URL is prose, not part of the link.
    #[test]
    fn trims_trailing_punctuation_and_unbalanced_brackets() {
        let cases = [
            ("go to https://luvus.dev.", "https://luvus.dev"),
            ("go to https://luvus.dev,", "https://luvus.dev"),
            ("(see https://luvus.dev)", "https://luvus.dev"),
            ("[see https://luvus.dev]", "https://luvus.dev"),
            // A parenthesis the URL opened itself is kept.
            (
                "see https://en.wikipedia.org/wiki/Foo_(bar)",
                "https://en.wikipedia.org/wiki/Foo_(bar)",
            ),
            // Query strings and fragments survive intact.
            (
                "see https://luvus.dev/a?b=1&c=2#frag!",
                "https://luvus.dev/a?b=1&c=2#frag",
            ),
        ];
        for (line, want) in cases {
            let col = line.find("https").unwrap() as u16 + 3;
            assert_eq!(
                url_at(&[line], 60, col, 0).as_deref(),
                Some(want),
                "{line:?}"
            );
        }
    }

    /// The reason this exists rather than leaning on the host terminal: a URL cut
    /// by the right edge is still one link.
    #[test]
    fn joins_a_url_wrapped_across_the_right_edge() {
        let g = grid(
            &["ref https://luvus.dev/docs/gui", "des/agents and then some"],
            30,
        );
        let link = link_at(&g, 20, 0).expect("found");
        assert_eq!(
            link.hit,
            Hit::Url("https://luvus.dev/docs/guides/agents".into())
        );
        assert_eq!(link.spans, vec![(0, 4, 30), (1, 0, 10)]);
        // Reachable from the continuation row too.
        assert_eq!(link_at(&g, 3, 1).map(|l| l.hit), Some(link.hit));
    }

    /// The join only happens when the row is genuinely full. A line that ends
    /// short is a finished line, so the next row must not be swallowed.
    #[test]
    fn does_not_join_across_a_line_that_ended_early() {
        let g = grid(&["ref https://luvus.dev", "docs/agents"], 30);
        let link = link_at(&g, 8, 0).expect("found");
        assert_eq!(link.hit, Hit::Url("https://luvus.dev".into()));
        assert_eq!(link.spans, vec![(0, 4, 21)]);
    }

    /// Paths come back unresolved, with any `:line[:col]` split off — the shape
    /// compilers and test runners print.
    #[test]
    fn finds_paths_and_splits_their_line_suffix() {
        let cases = [
            ("edit src/main.rs now", "src/main.rs", None),
            ("at src/main.rs:42 here", "src/main.rs", Some(42)),
            ("at src/main.rs:42:7 here", "src/main.rs", Some(42)),
            ("see /etc/hosts ok", "/etc/hosts", None),
            ("see ~/notes.md ok", "~/notes.md", None),
            ("see ./a/b.rs ok", "./a/b.rs", None),
            ("see ../up.toml ok", "../up.toml", None),
            // A bare filename counts when it carries an extension.
            ("open README.md now", "README.md", None),
            // Windows: the drive colon is never mistaken for a line suffix.
            ("at C:\\src\\main.rs ok", "C:\\src\\main.rs", None),
            ("at C:\\src\\main.rs:9 ok", "C:\\src\\main.rs", Some(9)),
            // Sentence punctuation is still trimmed.
            ("edit src/main.rs.", "src/main.rs", None),
            ("(edit src/main.rs)", "src/main.rs", None),
        ];
        for (line, want, want_ln) in cases {
            let col = line.find(' ').unwrap() as u16 + 2;
            assert_eq!(
                path_at(&[line], 60, col, 0),
                Some((want.to_string(), want_ln)),
                "{line:?}"
            );
        }
    }

    /// Prose with neither a separator nor an extension must never reach the
    /// filesystem: without this every hovered word would be stat-ed.
    ///
    /// The bar here is *shape*, not plausibility — `a.b.c` is a candidate because
    /// it could name a file. Whether it does is the app's existence check, which
    /// is the layer that keeps false positives off screen.
    #[test]
    fn plain_words_are_not_path_candidates() {
        for line in ["just some ordinary words", "run the tests now", "yes"] {
            for col in 0..line.len() as u16 {
                assert_eq!(
                    link_at(&grid(&[line], 40), col, 0),
                    None,
                    "{line:?} col {col}"
                );
            }
        }
    }

    /// Bare domains, so `luvus.dev` is as clickable as the written-out URL.
    #[test]
    fn recognises_bare_domains() {
        for ok in [
            "luvus.dev",
            "google.com",
            "luvus.dev/docs/guides",
            "sub.domain.co.uk",
            "bun.sh",
            "example.com:8080/x",
            "localhost:3000",
            "127.0.0.1:8080",
            "my-site.io",
        ] {
            assert_eq!(as_domain(ok), Some(ok), "{ok:?} should be a domain");
        }
    }

    /// The strictness that keeps this usable. Every one of these would otherwise
    /// offer to open a website.
    #[test]
    fn refuses_things_that_only_look_like_domains() {
        for bad in [
            // Upper case is the tell that it is a filename, not a domain. Both of
            // these are real TLDs (`.md` Moldova, `.toml` is not, `.rs` Serbia).
            "README.md",
            "Cargo.toml",
            "Makefile.am",
            // Unknown TLD.
            "notes.txt",
            "main.py",
            "data.json",
            // Not two labels, or malformed ones.
            "localhost",
            "com",
            ".com",
            "a..com",
            "-lead.com",
            "trail-.com",
            // A colon that is not a port.
            "host:abc",
            // Version numbers.
            "1.2",
            "v1.2.3",
        ] {
            assert_eq!(as_domain(bad), None, "{bad:?} must not be a domain");
        }
    }

    /// `localhost` is where dev servers live and they speak plain http; a real
    /// domain gets https.
    #[test]
    fn bare_domains_get_a_sensible_scheme() {
        assert_eq!(domain_url("luvus.dev"), "https://luvus.dev");
        assert_eq!(domain_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(domain_url("127.0.0.1:8080/x"), "http://127.0.0.1:8080/x");
    }

    /// A bare domain still reaches the app as a scheme-less hit, carrying the raw
    /// token so the port survives the line-suffix split.
    #[test]
    fn a_bare_domain_is_a_hit_with_its_port_intact() {
        let g = grid(&["serving on localhost:3000 now"], 40);
        match link_at(&g, 14, 0).expect("found").hit {
            Hit::Path { raw, .. } => assert_eq!(raw, "localhost:3000"),
            other => panic!("expected a scheme-less hit, got {other:?}"),
        }
        let g = grid(&["see luvus.dev/docs ok"], 40);
        match link_at(&g, 6, 0).expect("found").hit {
            Hit::Path { raw, .. } => assert_eq!(raw, "luvus.dev/docs"),
            other => panic!("expected a scheme-less hit, got {other:?}"),
        }
    }

    /// A scheme always wins: a URL is never mistaken for a path just because it
    /// contains slashes.
    #[test]
    fn a_url_is_never_read_as_a_path() {
        assert_eq!(
            path_at(&["see https://luvus.dev/a/b.rs ok"], 60, 10, 0),
            None
        );
        assert_eq!(
            url_at(&["see https://luvus.dev/a/b.rs ok"], 60, 10, 0).as_deref(),
            Some("https://luvus.dev/a/b.rs")
        );
    }

    #[test]
    fn covers_reports_the_cells_it_occupies() {
        let link = link_at(&grid(&["x https://luvus.dev"], 30), 5, 0).unwrap();
        assert!(link.covers(2, 0), "first cell of the URL");
        assert!(link.covers(18, 0), "last cell of the URL");
        assert!(!link.covers(1, 0), "the space before it");
        assert!(!link.covers(19, 0), "past the end");
        assert!(!link.covers(5, 1), "another row");
    }

    #[test]
    fn a_scheme_with_no_host_is_not_a_link() {
        assert_eq!(url_at(&["see https:// nothing"], 30, 6, 0), None);
        assert_eq!(url_at(&["see https:/// nothing"], 30, 6, 0), None);
    }

    /// Never panic, whatever the grid holds.
    #[test]
    fn out_of_range_and_odd_input_are_safe() {
        let g = grid(&["https://luvus.dev"], 20);
        assert_eq!(link_at(&g, 0, 9), None, "row past the end");
        assert_eq!(link_at(&g, 99, 0), None, "col past the end");
        assert_eq!(link_at(&[], 0, 0), None, "empty grid");
        assert_eq!(link_at(&["".to_string()], 0, 0), None, "empty row");
        // Wide glyphs and combining marks must not shift or split anything.
        let cjk = grid(&["日本語 https://luvus.dev 語"], 40);
        assert_eq!(
            link_at(&cjk, 6, 0).map(|l| l.hit),
            Some(Hit::Url("https://luvus.dev".into()))
        );
    }
}
