//! What the model is told about compression, and why the wording is load-bearing.
//!
//! A model only learns that elided data is fetchable from two places: the marker
//! it sees in a payload, and these strings. If the marker grammar is not spelled
//! out here, a `[... 412 similar items omitted [ref:k_AbC123] ...]` reads as
//! prose describing an absence rather than as an offer, the retrieval tool is
//! never called, and recoverable mode degrades into aggressive mode with extra
//! storage cost. So the shapes are listed literally, with the handle shown in
//! position, rather than described.

/// The `instructions` field of the `initialize` response.
///
/// This is the one piece of text every client is guaranteed to put in front of
/// the model, so it carries the whole contract: the marker shapes, the handle
/// grammar, the preference for a range, and the rule that a miss is regenerated
/// and never invented.
pub const SERVER_INSTRUCTIONS: &str = "\
OptScale Kompressor: optscale_kompressor is a context compressor. Large tool output and long \
conversation history that reach you may already have passed through it: \
whatever it removed was replaced by a short marker, and any marker carrying a \
handle can be read back in full with optscale_kompressor_retrieve.

A handle is always the text `ref:` followed by `k_` and exactly 12 url-safe \
characters (letters, digits, `-`, `_`), for example `ref:k_AbC123dEf456`. These \
are the marker shapes it appears in:

  [... 412 similar items omitted [ref:k_AbC123] (fields: id,status) ...]
      Omitted items of an array. The handle holds the omitted items.

  \u{2026}[+1840 chars ref:k_DeF456]
      A clipped string. The handle holds the characters after the ellipsis.

  <line> [\u{d7}37 ref:k_GhI789]
      A line that repeated exactly. The handle holds the folded occurrences, \
which are copies of the line shown.

  <line> [\u{d7}37 similar ref:k_GhI789]
      Thirty-seven lines that shared a shape; the one shown is the first. The \
other thirty-six DIFFER from it and from each other, and the handle is the \
only place they exist. Read this as an omission, not a repeat: if what \
distinguished them matters \u{2014} an identifier, a path, a step number, a \
quantity \u{2014} retrieve before answering. A bare [\u{d7}37] with no handle \
is the exact-repeat case and needs nothing.

  [... 87 log lines omitted [ref:k_\u{2026}]; 3 ERROR, 9 WARN]
      Dropped log, search, diff, table, listing or config lines. The handle \
holds exactly those lines. The same shape covers every one of those payload \
kinds; only the noun and the trailing summary change.

  # [... 31 body lines omitted from parse_row; retrieve: ref:k_\u{2026}]
      An elided function body. The handle holds the body. The comment character \
follows the language, so it may be `//` or `/* ... */` instead of `#`.

A marker with no `ref:` handle has nothing behind it: it was produced in \
aggressive mode, or the fragment was too large to retain. Do not call \
optscale_kompressor_retrieve for one.

How to use this:

  - When you need what a marker elided, call optscale_kompressor_retrieve with that \
handle. Never state or summarise what elided data contained unless you \
retrieved it.
  - Prefer a narrow `range`. Everything returned is re-injected into your next \
request and billed again, so an unnarrowed retrieval loop can cost more than \
the compression ever saved. Ask for the lines, the JSON pointer or the matching \
lines you actually need.
  - If a handle does not resolve, the data is gone. Regenerate it \u{2014} re-run the \
command, re-read the file \u{2014} and never reconstruct it from memory.

The remaining tools compress on demand: optscale_kompressor_compress for a \
conversation, optscale_kompressor_compress_raw for a single payload, \
optscale_kompressor_stats for what this \
tenant's compression has cost and saved.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_name_every_marker_shape_a_model_can_meet() {
        // A shape that is not named here is a shape the model reads as prose
        // about an absence, and never retrieves.
        for shape in [
            "similar items omitted",
            "chars ref:k_",
            "\u{d7}37 ref:",
            "log lines omitted",
            "body lines omitted from",
        ] {
            assert!(
                SERVER_INSTRUCTIONS.contains(shape),
                "instructions must name the {shape} marker"
            );
        }
    }

    #[test]
    fn the_instructions_state_the_handle_grammar_and_the_two_cost_rules() {
        assert!(SERVER_INSTRUCTIONS.contains("OptScale Kompressor"));
        assert!(SERVER_INSTRUCTIONS.contains("optscale_kompressor"));
        assert!(SERVER_INSTRUCTIONS.contains("exactly 12 url-safe characters"));
        assert!(SERVER_INSTRUCTIONS.contains("Prefer a narrow `range`"));
        assert!(SERVER_INSTRUCTIONS.contains("never reconstruct it from memory"));
    }
}
