use super::error_book::ErrorBook;

/// Error pattern matcher — delegates to ErrorBook for now
/// Future: add regex matching, fuzzy matching, LLM-based classification
impl ErrorBook {
    /// Check if an error is known and has a solution
    pub fn is_known_error(&self, error_msg: &str) -> bool {
        let error_lower = error_msg.to_lowercase();
        self.entries_ref().iter().any(|entry| {
            entry
                .patterns
                .iter()
                .any(|p| error_lower.contains(&p.to_lowercase()))
        })
    }

    /// Get pending entries that need LLM classification
    pub fn pending_entries(&self) -> Vec<&super::error_book::ErrorEntry> {
        self.entries_ref()
            .iter()
            .filter(|e| e.solution.contains("pending analysis"))
            .collect()
    }

    pub fn entries_ref(&self) -> &[super::error_book::ErrorEntry] {
        &self.entries
    }
}
