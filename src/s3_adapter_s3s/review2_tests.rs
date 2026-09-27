// SPDX-License-Identifier: BUSL-1.1

use super::*;

/// Review-2 (S5): browsers split Content-Type on commas and use the LAST
/// valid type (Fetch "extract a MIME type"). `image/png, text/html`
/// renders as HTML, but the essence here is taken before the first `;`,
/// so it counts as an inert image and gets no sandbox.
#[test]
fn review2_sandbox_is_not_bypassed_by_a_content_type_list() {
    for ct in [
        "image/png, text/html",
        "image/png;x=,text/html",
        "video/mp4,text/html",
    ] {
        assert!(
            content_type_needs_sandbox(ct),
            "{ct} renders as text/html but gets no sandbox"
        );
    }
}
