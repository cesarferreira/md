//! Find the first source line a save actually changed.

/// 1-based line in `after` where the text first differs from `before`.
///
/// The scan starts at the top, so an edit halfway down a file lands on that
/// edit rather than on a later line that merely shifted.
pub fn first_changed_line(before: &str, after: &str) -> Option<u32> {
    let old = lines(before);
    let new = lines(after);
    let shared = old.len().min(new.len());
    for index in 0..shared {
        if old[index] != new[index] {
            return Some(index as u32 + 1);
        }
    }
    if old.len() == new.len() || new.is_empty() {
        return None;
    }
    let index = if new.len() > old.len() {
        shared
    } else {
        shared.saturating_sub(1)
    };
    Some(index as u32 + 1)
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_in_the_middle_is_that_line() {
        let before = "# Title\n\nKeep this.\n\nEdit me.\n";
        let after = "# Title\n\nKeep this.\n\nEdited.\n";
        assert_eq!(first_changed_line(before, after), Some(5));
    }

    #[test]
    fn an_insertion_at_the_top_is_line_one() {
        let before = "Keep\n";
        let after = "New\n\nKeep\n";
        assert_eq!(first_changed_line(before, after), Some(1));
    }

    #[test]
    fn a_line_appended_at_the_end_is_that_line() {
        let before = "One\nTwo\n";
        let after = "One\nTwo\nThree\n";
        assert_eq!(first_changed_line(before, after), Some(3));
    }

    #[test]
    fn a_deleted_trailing_line_lands_on_what_remains() {
        let before = "One\nTwo\nThree\n";
        let after = "One\nTwo\n";
        assert_eq!(first_changed_line(before, after), Some(2));
    }

    #[test]
    fn identical_text_has_no_change() {
        assert_eq!(first_changed_line("Same\n", "Same\n"), None);
        assert_eq!(first_changed_line("Same\n", "Same"), None);
    }
}
