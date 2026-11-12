//! The logo is defined once and copied into a few places.
//!
//! `service/docs/assets/logo.svg` is the canonical file. The documentation site inlines
//! it and the dashboard frontend embeds it as a string — if one of them is edited without
//! the others, the project ends up with two different paws. This test makes that a build
//! failure instead of a design smell.

use std::path::PathBuf;

/// The repository root (this crate lives in `<root>/service`).
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate lives in the repository")
        .to_path_buf()
}

/// The documentation site — it lives **in this crate**, because it is embedded in
/// the binary.
fn docs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs")
}

/// The shapes of the canonical logo: everything between `<g ...>` and `</g>`.
fn canonical_shapes() -> String {
    let path = docs().join("assets/logo.svg");
    let svg = std::fs::read_to_string(&path).expect("docs/assets/logo.svg exists");
    let start = svg.find("<g ").expect("the logo has a group");
    let end = svg.find("</g>").expect("the logo has a closed group") + "</g>".len();
    svg[start..end].to_string()
}

/// The accessible name of the canonical logo: the `aria-label` a screen reader
/// announces. It is part of the mark — the paw *and* the name it is called — so
/// it travels with the shapes.
fn canonical_label() -> String {
    let path = docs().join("assets/logo.svg");
    let svg = std::fs::read_to_string(&path).expect("docs/assets/logo.svg exists");
    let start = svg
        .find("aria-label=\"")
        .expect("the logo has an aria-label")
        + "aria-label=\"".len();
    let end = svg[start..].find('"').expect("the aria-label is closed") + start;
    svg[start..end].to_string()
}

/// Every page of the documentation site, in every section.
fn html_pages(dir: &PathBuf) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(html_pages(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("html") {
            found.push(path);
        }
    }
    found
}

/// Every copy of the logo in the repository must match the canonical file.
#[test]
fn every_logo_copy_matches_the_canonical_file() {
    let shapes = canonical_shapes();
    let label = format!("aria-label=\"{}\"", canonical_label());

    // The documentation site — every page of it, in every section.
    let docs = docs();
    let mut pages = 0;
    for path in html_pages(&docs) {
        let html = std::fs::read_to_string(&path).expect("a readable page");
        assert!(
            html.contains(&shapes),
            "{} does not use the canonical logo — copy it from docs/assets/logo.svg",
            path.display()
        );
        assert!(
            html.contains(&label),
            "{} names the logo differently — copy the aria-label from docs/assets/logo.svg",
            path.display()
        );
        pages += 1;
    }
    assert!(pages >= 8, "expected the whole documentation site, found {pages} pages");

    // The dashboard frontend.
    let app = root().join("js/sumimi-app.js");
    let js = std::fs::read_to_string(&app).expect("js/sumimi-app.js exists");
    assert!(
        js.contains(&shapes),
        "js/sumimi-app.js does not use the canonical logo — copy it from docs/assets/logo.svg"
    );
    assert!(
        js.contains(&label),
        "js/sumimi-app.js names the logo differently — copy the aria-label from docs/assets/logo.svg"
    );

    // The tracker must not carry a logo of its own.
    let tracker = root().join("service/js/sumimi.js");
    let js = std::fs::read_to_string(&tracker).expect("js/sumimi.js exists");
    assert!(
        !js.contains("currentColor"),
        "the tracker should not carry branding"
    );
}
