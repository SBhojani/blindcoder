//! Announce the compiled-in non-ZDR routing capability in the build log: the build feature is the
//! first of the consent gates, and its presence must be visible, never silent.

fn main() {
    if std::env::var_os("CARGO_FEATURE_ALLOW_NON_ZDR").is_some() {
        println!(
            "cargo:warning=blindcoder: built WITH `allow-non-zdr` — the non-ZDR / pay-with-data \
             routing path is compiled in (dormant unless configured and attested at startup)."
        );
    }
}
