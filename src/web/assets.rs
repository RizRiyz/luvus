pub(super) struct Asset {
    pub body: &'static [u8],
    pub content_type: &'static str,
    pub immutable: bool,
}

pub(super) fn get(path: &str) -> Asset {
    match path {
        "/app.js" => Asset {
            body: include_bytes!("assets/app.js"),
            content_type: "text/javascript; charset=utf-8",
            immutable: false,
        },
        "/app.css" => Asset {
            body: include_bytes!("assets/app.css"),
            content_type: "text/css; charset=utf-8",
            immutable: false,
        },
        "/mark.svg" => Asset {
            body: include_bytes!("assets/mark.svg"),
            content_type: "image/svg+xml",
            immutable: false,
        },
        "/manifest.webmanifest" => Asset {
            body: include_bytes!("assets/manifest.webmanifest"),
            content_type: "application/manifest+json",
            immutable: false,
        },
        "/sw.js" => Asset {
            body: include_bytes!("assets/sw.js"),
            content_type: "text/javascript; charset=utf-8",
            immutable: false,
        },
        _ => Asset {
            body: include_bytes!("assets/index.html"),
            content_type: "text/html; charset=utf-8",
            immutable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_index_references_embedded_assets() {
        let index = String::from_utf8_lossy(get("/").body);
        assert!(index.contains("/app.js"));
        assert!(index.contains("/app.css"));
        assert!(!get("/app.js").body.is_empty());
        assert!(!get("/app.css").body.is_empty());
    }
}
