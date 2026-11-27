//! Regressions from the third review (of 0.4.0 and 0.4.1).

use mh_hooks::brief::neutralise;

/// A line of `-` or `=` under third-party text would make that text a
/// Markdown heading (setext), and alone it is a rule: both are bracketed.
#[test]
fn setext_underlines_and_rules_are_neutralised() {
    assert_eq!(neutralise("---------------"), "[---------------]");
    assert_eq!(neutralise("  ===  "), "  [===]");
    assert_eq!(neutralise("* * *"), "[* * *]");
    assert_eq!(neutralise("___"), "[___]");
    // Ordinary lines pass through.
    assert_eq!(neutralise("- a list item"), "- a list item");
    assert_eq!(neutralise("a -- b"), "a -- b");
    assert_eq!(neutralise(""), "");
    assert_eq!(neutralise("   "), "   ");
}
