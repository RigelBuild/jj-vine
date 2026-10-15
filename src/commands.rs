use unicode_segmentation::UnicodeSegmentation as _;

pub mod init;
pub mod status;
pub mod submit;

pub enum GetBookmarksOptions {
    /// Use a manual revset.
    Revset(String),

    /// Include only `(mine() & tracked_remote_bookmarks())`, minus merged
    /// ancestry: `trunk()` and, when set, the remote `defaultBaseBranch`.
    Tracked,

    /// Include only `(mine() & bookmarks())`, minus merged ancestry: `trunk()`
    /// and, when set, the remote `defaultBaseBranch`.
    Mine,
}

impl GetBookmarksOptions {
    #[must_use]
    pub fn to_revset(&self, merged_boundary_revset: &str) -> String {
        match self {
            GetBookmarksOptions::Revset(revset) => revset.clone(),
            GetBookmarksOptions::Tracked => {
                format!("(mine() & tracked_remote_bookmarks()) ~ ::({merged_boundary_revset})")
            }
            GetBookmarksOptions::Mine => {
                format!("(mine() & bookmarks()) ~ ::({merged_boundary_revset})")
            }
        }
    }
}

trait StrVisualWidth {
    fn visual_width(&self) -> usize;
}

impl<T> StrVisualWidth for T
where
    T: AsRef<str>,
{
    fn visual_width(&self) -> usize {
        strip_ansi::strip_str(self.as_ref()).graphemes(true).count()
    }
}
