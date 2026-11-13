pub mod find;
pub mod grep;
pub mod ls;

/// Walk `root` the way the find and grep tools search: skip hidden files
/// and anything git would ignore, also outside a git repository.
fn search_walker(root: &std::path::Path) -> ignore::WalkBuilder {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .require_git(false);
    walker
}
