//! blindcoder **alias** — masking by random *stored* tokens, not hashes.
//!
//! A hash of the model name is **not** blinding: the candidate pool is your own small known
//! list, so `sha256(slug)` over the pool unmasks instantly. Instead each provider and each
//! canonical model key gets a random token, minted once and stored. The same model under two
//! providers shares its model-token (`x7k2:q4m9` vs `b3wp:q4m9`), which keeps cross-provider
//! matching while hiding identity.
//!
//! The real slug lives only behind the [`RevealGate`]: peeking is a deliberate, loggable act,
//! because seeing the identity biases your future ratings.

use rand::Rng;

const TOKEN_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Default token length (e.g. `x7k2`).
pub const TOKEN_LEN: usize = 4;

/// A minted alias: `provider_token:model_token`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Alias {
    pub provider_token: String,
    pub model_token: String,
}

impl Alias {
    /// The display form shown in place of the real name, e.g. `x7k2:q4m9`.
    pub fn display(&self) -> String {
        format!("{}:{}", self.provider_token, self.model_token)
    }
}

/// Mint a fresh random lowercase-alphanumeric token.
pub fn mint_token<R: Rng + ?Sized>(rng: &mut R, len: usize) -> String {
    (0..len)
        .map(|_| TOKEN_ALPHABET[rng.gen_range(0..TOKEN_ALPHABET.len())] as char)
        .collect()
}

/// The reveal seam. Unmasking always goes through here so it can be logged/audited; a bare
/// alias→slug map is never handed around in the clear.
pub struct RevealGate;

/// Why an unmask happened — recorded alongside the reveal so peeks stay visible in the history.
#[derive(Clone, Copy, Debug)]
pub enum RevealReason {
    /// The user explicitly asked to see the identity (`reveal` subcommand).
    UserRequested,
    /// Internal routing needs the real slug to forward the request (not user-visible).
    Routing,
}

impl RevealGate {
    /// Resolve an alias to its real identity via the caller-supplied lookup, tagging the reason. In
    /// M1+ this is where the reveal is journaled; at M0 it centralizes the single crossing point so
    /// nothing else touches the mapping directly.
    ///
    /// Generic over the unmasked payload `T` so one gate serves every crossing: a user-facing
    /// reveal wants the real-slug `String`, routing wants the full routing target (`Route`), and the
    /// gate need not know the difference. Keeping the payload type at the call site is what lets the
    /// alias crate stay decoupled from `store` while still funnelling every unmask through here.
    pub fn reveal<T, F>(&self, alias: &Alias, reason: RevealReason, lookup: F) -> Option<T>
    where
        F: FnOnce(&Alias) -> Option<T>,
    {
        let _ = reason;
        lookup(alias)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn tokens_are_lowercase_alnum_of_requested_length() {
        let mut rng = StdRng::seed_from_u64(7);
        let tok = mint_token(&mut rng, TOKEN_LEN);
        assert_eq!(tok.len(), TOKEN_LEN);
        assert!(tok.bytes().all(|b| TOKEN_ALPHABET.contains(&b)));
    }

    #[test]
    fn reveal_goes_through_the_gate() {
        let a = Alias { provider_token: "x7k2".into(), model_token: "q4m9".into() };
        let got = RevealGate.reveal(&a, RevealReason::UserRequested, |al| {
            (al.model_token == "q4m9").then(|| "acme/model-x".to_string())
        });
        assert_eq!(got.as_deref(), Some("acme/model-x"));
        assert_eq!(a.display(), "x7k2:q4m9");
    }
}
