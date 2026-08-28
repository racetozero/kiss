//! Cached, asynchronous file and directory search for editor `@` mentions.

use kiss_tui::fuzzy::PreparedFuzzyQuery;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

const CACHE_ROOTS: usize = 2;
const INDEX_TTL: Duration = Duration::from_secs(30);
const MAX_INDEX_ENTRIES: usize = 500_000;
const MAX_RESULTS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSearchMatch {
    pub(crate) path: String,
    pub(crate) is_directory: bool,
    pub(crate) quoted: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FileSearchResult {
    pub(crate) request_id: u64,
    pub(crate) prefix: String,
    pub(crate) values: Vec<FileSearchMatch>,
    pub(crate) index_limited: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FileSearchQuery {
    pub(crate) prefix: String,
    pub(crate) root: PathBuf,
    pub(crate) query: String,
    pub(crate) display_base: String,
    pub(crate) quoted: bool,
}

impl FileSearchQuery {
    pub(crate) fn from_prefix(cwd: &Path, home: Option<&Path>, prefix: &str) -> Option<Self> {
        let (raw_query, quoted) = prefix
            .strip_prefix("@\"")
            .map(|query| (query, true))
            .unwrap_or_else(|| (prefix.strip_prefix('@').unwrap_or(prefix), false));
        let normalized = raw_query.replace('\\', "/");

        if normalized == "~" {
            return Some(Self {
                prefix: prefix.to_string(),
                root: lexical_normalize(home?.to_path_buf()),
                query: String::new(),
                display_base: "~/".into(),
                quoted,
            });
        }

        if let Some(slash) = normalized.rfind('/') {
            let display_base = normalized[..=slash].to_string();
            let query = normalized[slash + 1..].to_string();
            let unresolved = if let Some(relative) = display_base.strip_prefix("~/") {
                home?.join(relative)
            } else if display_base.starts_with('/') {
                PathBuf::from(&display_base)
            } else {
                cwd.join(&display_base)
            };
            return Some(Self {
                prefix: prefix.to_string(),
                root: lexical_normalize(unresolved),
                query,
                display_base,
                quoted,
            });
        }

        Some(Self {
            prefix: prefix.to_string(),
            root: lexical_normalize(cwd.to_path_buf()),
            query: normalized,
            display_base: String::new(),
            quoted,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchTicket {
    pub(crate) request_id: u64,
    pub(crate) indexing: bool,
}

pub(crate) struct FileSearchService {
    shared: Arc<SharedState>,
    result_tx: mpsc::UnboundedSender<FileSearchResult>,
    current_search: Option<CancellationToken>,
    next_request_id: u64,
}

struct SharedState {
    cache: Mutex<IndexCache>,
    previous_search: Mutex<Option<PreviousSearch>>,
}

struct PreviousSearch {
    index: Weak<FileIndex>,
    query: String,
    candidates: Arc<Vec<u32>>,
}

#[derive(Default)]
struct IndexCache {
    roots: HashMap<PathBuf, Arc<RootIndex>>,
    lru: VecDeque<PathBuf>,
}

struct RootIndex {
    root: PathBuf,
    state: Mutex<RootIndexState>,
    ready: Notify,
    scan_cancel: CancellationToken,
}

struct RootIndexState {
    index: Option<Arc<FileIndex>>,
    indexed_at: Option<Instant>,
    loading: bool,
}

pub(crate) struct FileIndex {
    paths: Box<str>,
    entries: Vec<IndexEntry>,
    default_order: Vec<RankedIndex>,
    limited: bool,
}

pub(crate) struct IndexedPath {
    relative: Box<str>,
    is_directory: bool,
    ascii_mask: Option<u64>,
}

struct IndexEntry {
    offset: u32,
    len: u32,
    is_directory: bool,
    ascii_mask: Option<u64>,
}

impl IndexedPath {
    pub(crate) fn new(relative: impl Into<Box<str>>, is_directory: bool) -> Self {
        let relative = relative.into();
        Self {
            ascii_mask: relative.is_ascii().then(|| path_ascii_mask(&relative)),
            relative,
            is_directory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RankedIndex {
    score: i64,
    is_directory: bool,
    path_len: usize,
    index: usize,
}

impl Ord for RankedIndex {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| self.is_directory.cmp(&other.is_directory))
            .then_with(|| other.path_len.cmp(&self.path_len))
            .then_with(|| other.index.cmp(&self.index))
    }
}

impl PartialOrd for RankedIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl FileIndex {
    pub(crate) fn new(entries: Vec<IndexedPath>, limited: bool) -> Self {
        let mut paths =
            String::with_capacity(entries.iter().map(|entry| entry.relative.len()).sum());
        let mut compact = Vec::with_capacity(entries.len());
        for entry in entries {
            let offset = u32::try_from(paths.len()).expect("file index text stays below 4 GiB");
            let len = u32::try_from(entry.relative.len()).expect("one path stays below 4 GiB");
            paths.push_str(&entry.relative);
            compact.push(IndexEntry {
                offset,
                len,
                is_directory: entry.is_directory,
                ascii_mask: entry.ascii_mask,
            });
        }
        let mut index = Self {
            paths: paths.into_boxed_str(),
            entries: compact,
            default_order: Vec::new(),
            limited,
        };
        index.default_order = rank_entries(&index, None, None).unwrap_or_default();
        index
    }

    fn path(&self, entry: &IndexEntry) -> &str {
        let start = entry.offset as usize;
        &self.paths[start..start + entry.len as usize]
    }

    #[cfg(test)]
    pub(crate) fn search(
        &self,
        query: &FileSearchQuery,
        cancellation: Option<&CancellationToken>,
    ) -> Option<Vec<FileSearchMatch>> {
        let prepared = (!query.query.is_empty()).then(|| PreparedFuzzyQuery::new(&query.query));
        let ranked = if prepared.is_none() {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return None;
            }
            self.default_order.clone()
        } else {
            rank_entries(self, prepared.as_ref(), cancellation)?
        };
        Some(self.matches(query, ranked))
    }

    fn search_reusing(
        &self,
        query: &FileSearchQuery,
        candidates: Option<&[u32]>,
        cancellation: Option<&CancellationToken>,
    ) -> Option<(Vec<FileSearchMatch>, Arc<Vec<u32>>)> {
        if query.query.is_empty() {
            return Some((
                self.matches(query, self.default_order.clone()),
                Arc::new(Vec::new()),
            ));
        }
        let prepared = PreparedFuzzyQuery::new(&query.query);
        let (ranked, matched) =
            rank_entries_from(self, Some(&prepared), cancellation, candidates, true)?;
        Some((self.matches(query, ranked), Arc::new(matched)))
    }

    fn matches(&self, query: &FileSearchQuery, ranked: Vec<RankedIndex>) -> Vec<FileSearchMatch> {
        ranked
            .into_iter()
            .map(|ranked| {
                let entry = &self.entries[ranked.index];
                let mut path = scoped_file_display(&query.display_base, self.path(entry));
                if entry.is_directory {
                    path.push('/');
                }
                FileSearchMatch {
                    path,
                    is_directory: entry.is_directory,
                    quoted: query.quoted,
                }
            })
            .collect()
    }
}

impl FileSearchService {
    pub(crate) fn new(result_tx: mpsc::UnboundedSender<FileSearchResult>) -> Self {
        Self {
            shared: Arc::new(SharedState {
                cache: Mutex::new(IndexCache::default()),
                previous_search: Mutex::new(None),
            }),
            result_tx,
            current_search: None,
            next_request_id: 0,
        }
    }

    pub(crate) fn warm(&self, root: PathBuf) {
        ensure_root(&self.shared, lexical_normalize(root));
    }

    pub(crate) fn search(&mut self, query: FileSearchQuery) -> SearchTicket {
        self.cancel();
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request_id = self.next_request_id;
        let cancellation = CancellationToken::new();
        self.current_search = Some(cancellation.clone());

        let (root_index, indexing) = ensure_root(&self.shared, query.root.clone());
        let result_tx = self.result_tx.clone();
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let Some(index) = wait_for_index(&root_index, &cancellation).await else {
                return;
            };
            let reusable = shared
                .previous_search
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|previous| {
                    previous.index.upgrade().and_then(|previous_index| {
                        (Arc::ptr_eq(&previous_index, &index)
                            && query.query.len() > previous.query.len()
                            && query.query.starts_with(&previous.query)
                            && previous.candidates.len() * 4 < index.entries.len() * 3)
                            .then(|| previous.candidates.clone())
                    })
                });
            let search_query = query.clone();
            let search_cancel = cancellation.clone();
            let search_index = index.clone();
            let searched = tokio::task::spawn_blocking(move || {
                let (values, candidates) = search_index.search_reusing(
                    &search_query,
                    reusable.as_deref().map(Vec::as_slice),
                    Some(&search_cancel),
                )?;
                Some((values, candidates, search_index.limited))
            })
            .await
            .ok()
            .flatten();
            let Some((values, candidates, index_limited)) = searched else {
                return;
            };
            if cancellation.is_cancelled() {
                return;
            }
            *shared.previous_search.lock().unwrap() =
                (!query.query.is_empty()).then(|| PreviousSearch {
                    index: Arc::downgrade(&index),
                    query: query.query.clone(),
                    candidates,
                });
            let _ = result_tx.send(FileSearchResult {
                request_id,
                prefix: query.prefix,
                values,
                index_limited,
            });
        });

        SearchTicket {
            request_id,
            indexing,
        }
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(cancellation) = self.current_search.take() {
            cancellation.cancel();
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_all(&mut self) {
        self.cancel();
        let mut cache = self.shared.cache.lock().unwrap();
        for root in cache.roots.values() {
            root.scan_cancel.cancel();
        }
        cache.roots.clear();
        cache.lru.clear();
        *self.shared.previous_search.lock().unwrap() = None;
    }

    /// Refresh cached roots in the background while old results stay usable.
    pub(crate) fn refresh_all(&mut self) {
        self.cancel();
        *self.shared.previous_search.lock().unwrap() = None;
        let roots = self
            .shared
            .cache
            .lock()
            .unwrap()
            .roots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for root in roots {
            let start = {
                let mut state = root.state.lock().unwrap();
                if state.loading {
                    false
                } else {
                    state.loading = true;
                    true
                }
            };
            if start {
                start_index_scan(root);
            }
        }
    }
}

fn ensure_root(shared: &Arc<SharedState>, root: PathBuf) -> (Arc<RootIndex>, bool) {
    let (entry, start_scan, indexing) = {
        let mut cache = shared.cache.lock().unwrap();
        if let Some(entry) = cache.roots.get(&root).cloned() {
            touch_root(&mut cache.lru, &root);
            let mut state = entry.state.lock().unwrap();
            let expired = state
                .indexed_at
                .is_some_and(|time| time.elapsed() >= INDEX_TTL);
            let start_scan = expired && !state.loading;
            if start_scan {
                state.loading = true;
            }
            let indexing = state.index.is_none();
            drop(state);
            (entry, start_scan, indexing)
        } else {
            let entry = Arc::new(RootIndex {
                root: root.clone(),
                state: Mutex::new(RootIndexState {
                    index: None,
                    indexed_at: None,
                    loading: true,
                }),
                ready: Notify::new(),
                scan_cancel: CancellationToken::new(),
            });
            cache.roots.insert(root.clone(), entry.clone());
            touch_root(&mut cache.lru, &root);
            while cache.roots.len() > CACHE_ROOTS {
                if let Some(expired) = cache.lru.pop_front()
                    && let Some(removed) = cache.roots.remove(&expired)
                {
                    removed.scan_cancel.cancel();
                }
            }
            (entry, true, true)
        }
    };

    if start_scan {
        start_index_scan(entry.clone());
    }
    (entry, indexing)
}

fn touch_root(lru: &mut VecDeque<PathBuf>, root: &Path) {
    if let Some(position) = lru.iter().position(|candidate| candidate == root) {
        lru.remove(position);
    }
    lru.push_back(root.to_path_buf());
}

fn start_index_scan(root_index: Arc<RootIndex>) {
    tokio::task::spawn_blocking(move || {
        let index = build_index(&root_index.root, Some(&root_index.scan_cancel)).map(Arc::new);
        let mut state = root_index.state.lock().unwrap();
        state.index = index;
        state.indexed_at = state.index.as_ref().map(|_| Instant::now());
        state.loading = false;
        drop(state);
        root_index.ready.notify_waiters();
    });
}

async fn wait_for_index(
    root_index: &Arc<RootIndex>,
    cancellation: &CancellationToken,
) -> Option<Arc<FileIndex>> {
    loop {
        let notified = root_index.ready.notified();
        if let Some(index) = root_index.state.lock().unwrap().index.clone() {
            return Some(index);
        }
        tokio::select! {
            _ = notified => {}
            _ = cancellation.cancelled() => return None,
        }
    }
}

fn build_index(root: &Path, cancellation: Option<&CancellationToken>) -> Option<FileIndex> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut entries = Vec::new();
    let mut limited = false;
    for (walk_index, entry) in builder.build().filter_map(Result::ok).enumerate() {
        if walk_index % 256 == 0 && cancellation.is_some_and(CancellationToken::is_cancelled) {
            return None;
        }
        let path = entry.path();
        if path == root {
            continue;
        }
        if entries.len() == MAX_INDEX_ENTRIES {
            limited = true;
            break;
        }
        let Some(relative) = path.strip_prefix(root).ok() else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
        entries.push(IndexedPath::new(relative, is_directory));
    }
    Some(FileIndex::new(entries, limited))
}

fn rank_entries(
    index: &FileIndex,
    query: Option<&PreparedFuzzyQuery>,
    cancellation: Option<&CancellationToken>,
) -> Option<Vec<RankedIndex>> {
    rank_entries_from(index, query, cancellation, None, false).map(|(ranked, _)| ranked)
}

fn rank_entries_from(
    file_index: &FileIndex,
    query: Option<&PreparedFuzzyQuery>,
    cancellation: Option<&CancellationToken>,
    candidates: Option<&[u32]>,
    collect_matches: bool,
) -> Option<(Vec<RankedIndex>, Vec<u32>)> {
    let candidate_count = candidates.map_or(file_index.entries.len(), <[u32]>::len);
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4);
    let chunks = if candidate_count >= 150_000 && workers > 1 {
        let chunk_size = candidate_count.div_ceil(workers);
        std::thread::scope(|scope| {
            let handles = (0..workers)
                .filter_map(|worker| {
                    let start = worker * chunk_size;
                    let end = (start + chunk_size).min(candidate_count);
                    (start < end).then(|| {
                        scope.spawn(move || {
                            rank_chunk(
                                file_index,
                                query,
                                cancellation,
                                candidates,
                                collect_matches,
                                start,
                                end,
                            )
                        })
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().ok().flatten())
                .collect::<Option<Vec<_>>>()
        })?
    } else {
        vec![rank_chunk(
            file_index,
            query,
            cancellation,
            candidates,
            collect_matches,
            0,
            candidate_count,
        )?]
    };

    let mut best = BinaryHeap::<Reverse<RankedIndex>>::with_capacity(MAX_RESULTS + 1);
    let mut matched = collect_matches.then(Vec::new).unwrap_or_default();
    for chunk in chunks {
        for candidate in chunk.best.into_iter().map(|entry| entry.0) {
            push_ranked(&mut best, candidate);
        }
        matched.extend(chunk.matched);
    }
    let mut ranked = best.into_iter().map(|entry| entry.0).collect::<Vec<_>>();
    sort_ranked(file_index, &mut ranked);
    Some((ranked, matched))
}

struct RankedChunk {
    best: BinaryHeap<Reverse<RankedIndex>>,
    matched: Vec<u32>,
}

#[allow(clippy::too_many_arguments)]
fn rank_chunk(
    file_index: &FileIndex,
    query: Option<&PreparedFuzzyQuery>,
    cancellation: Option<&CancellationToken>,
    candidates: Option<&[u32]>,
    collect_matches: bool,
    start: usize,
    end: usize,
) -> Option<RankedChunk> {
    let mut best = BinaryHeap::with_capacity(MAX_RESULTS + 1);
    let mut matched = collect_matches.then(Vec::new).unwrap_or_default();
    let required_mask = query.and_then(PreparedFuzzyQuery::required_ascii_mask);
    for candidate_position in start..end {
        if candidate_position % 256 == 0
            && cancellation.is_some_and(CancellationToken::is_cancelled)
        {
            return None;
        }
        let index = candidates
            .map(|candidates| candidates[candidate_position] as usize)
            .unwrap_or(candidate_position);
        let entry = &file_index.entries[index];
        let path = file_index.path(entry);
        if let (Some(required), Some(candidate)) = (required_mask, entry.ascii_mask)
            && candidate & required != required
        {
            continue;
        }
        let score = match query {
            Some(query) => {
                let Some(score) = query.score(path) else {
                    continue;
                };
                score
            }
            None => 1,
        };
        if collect_matches {
            matched.push(index as u32);
        }
        let candidate = RankedIndex {
            score,
            is_directory: entry.is_directory,
            path_len: path.len(),
            index,
        };
        push_ranked(&mut best, candidate);
    }
    Some(RankedChunk { best, matched })
}

fn push_ranked(best: &mut BinaryHeap<Reverse<RankedIndex>>, candidate: RankedIndex) {
    if best.len() < MAX_RESULTS {
        best.push(Reverse(candidate));
    } else if best
        .peek()
        .is_some_and(|current_worst| candidate > current_worst.0)
    {
        best.pop();
        best.push(Reverse(candidate));
    }
}

fn sort_ranked(file_index: &FileIndex, ranked: &mut [RankedIndex]) {
    let entries = &file_index.entries;
    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.is_directory.cmp(&left.is_directory))
            .then_with(|| left.path_len.cmp(&right.path_len))
            .then_with(|| {
                file_index
                    .path(&entries[left.index])
                    .cmp(file_index.path(&entries[right.index]))
            })
    });
}

fn path_ascii_mask(path: &str) -> u64 {
    path.bytes().fold(0u64, |mask, byte| {
        let lower = byte.to_ascii_lowercase();
        let bit = match lower {
            b'a'..=b'z' => Some(lower - b'a'),
            b'0'..=b'9' => Some(26 + lower - b'0'),
            _ => None,
        };
        bit.map_or(mask, |bit| mask | (1u64 << bit))
    })
}

fn scoped_file_display(display_base: &str, relative: &str) -> String {
    if display_base == "/" {
        format!("/{relative}")
    } else {
        format!("{display_base}{relative}")
    }
}

fn lexical_normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(root: &Path, prefix: &str, query: &str) -> FileSearchQuery {
        FileSearchQuery {
            prefix: prefix.into(),
            root: root.to_path_buf(),
            query: query.into(),
            display_base: String::new(),
            quoted: false,
        }
    }

    #[test]
    fn prefix_resolution_preserves_home_and_parent_spelling() {
        let root = Path::new("/tmp/kiss-search");
        let home = root.join("home");
        let cwd = root.join("work/one/two");

        let home_query = FileSearchQuery::from_prefix(&cwd, Some(&home), "@~/src/ma").unwrap();
        assert_eq!(home_query.root, home.join("src"));
        assert_eq!(home_query.display_base, "~/src/");
        assert_eq!(home_query.query, "ma");

        let parent_query =
            FileSearchQuery::from_prefix(&cwd, Some(&home), "@../../ancestor").unwrap();
        assert_eq!(parent_query.root, root.join("work"));
        assert_eq!(parent_query.display_base, "../../");
        assert_eq!(parent_query.query, "ancestor");
    }

    #[test]
    fn index_is_recursive_and_respects_gitignore() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(temp.path().join("ignored.rs"), "ignored").unwrap();
        std::fs::write(nested.join("main.rs"), "fn main() {}\n").unwrap();

        let index = build_index(temp.path(), None).unwrap();
        let values = index
            .search(&query(temp.path(), "@main", "main"), None)
            .unwrap();

        assert!(
            values
                .iter()
                .any(|value| value.path == "src/nested/main.rs")
        );
        assert!(values.iter().all(|value| value.path != "ignored.rs"));
        let all_values = index.search(&query(temp.path(), "@", ""), None).unwrap();
        assert!(
            all_values
                .iter()
                .any(|value| value.is_directory && value.path.ends_with('/'))
        );
    }

    #[test]
    fn reused_prefix_candidates_match_a_full_search() {
        let entries = (0..10_000)
            .map(|index| {
                IndexedPath::new(
                    format!("src/module_{:04}/component_{index:05}.rs", index / 100),
                    false,
                )
            })
            .collect();
        let index = FileIndex::new(entries, false);
        let (_, candidates) = index
            .search_reusing(
                &query(Path::new("/synthetic"), "@component9", "component9"),
                None,
                None,
            )
            .unwrap();
        let reused = index
            .search_reusing(
                &query(Path::new("/synthetic"), "@component99", "component99"),
                Some(&candidates),
                None,
            )
            .unwrap()
            .0;
        let full = index
            .search(
                &query(Path::new("/synthetic"), "@component99", "component99"),
                None,
            )
            .unwrap();

        assert_eq!(reused, full);
    }

    #[test]
    fn empty_reused_search_uses_the_default_order() {
        let index = FileIndex::new(
            vec![
                IndexedPath::new("src/long/path.rs", false),
                IndexedPath::new("a.rs", false),
                IndexedPath::new("src/", true),
            ],
            false,
        );
        let query = query(Path::new("/synthetic"), "@", "");
        let full = index.search(&query, None).unwrap();
        let (reused, candidates) = index.search_reusing(&query, None, None).unwrap();

        assert_eq!(reused, full);
        assert!(candidates.is_empty());
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_file_search() {
        for path_count in [10_000, 100_000, 500_000] {
            let entries = (0..path_count)
                .map(|index| {
                    IndexedPath::new(
                        format!(
                            "src/module_{:06}/component_{:06}_handler.rs",
                            index / 100,
                            index
                        ),
                        false,
                    )
                })
                .collect();
            let index = FileIndex::new(entries, false);
            kiss_bench::measure(
                &format!("file_search_{path_count}"),
                15,
                1,
                "three_warm_queries_max_100_results",
                || {
                    ["handler", "module42", "component999"]
                        .into_iter()
                        .map(|query_text| {
                            index
                                .search(&query(Path::new("/synthetic"), "@query", query_text), None)
                                .unwrap()
                                .len()
                        })
                        .sum::<usize>()
                },
            );
            kiss_bench::measure(
                &format!("file_search_prefix_{path_count}"),
                15,
                1,
                "five_extended_prefix_queries",
                || {
                    let mut candidates: Option<Arc<Vec<u32>>> = None;
                    ["c", "component9", "component99", "component999"]
                        .into_iter()
                        .map(|query_text| {
                            let (values, next_candidates) = index
                                .search_reusing(
                                    &query(Path::new("/synthetic"), "@query", query_text),
                                    candidates.as_deref().map(Vec::as_slice),
                                    None,
                                )
                                .unwrap();
                            candidates = (next_candidates.len() * 4 < index.entries.len() * 3)
                                .then_some(next_candidates);
                            values.len()
                        })
                        .sum::<usize>()
                },
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn newer_search_cancels_an_old_result() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("alpha.rs"), "alpha").unwrap();
        std::fs::write(temp.path().join("beta.rs"), "beta").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut service = FileSearchService::new(tx);

        service.search(query(temp.path(), "@alpha", "alpha"));
        let second = service.search(query(temp.path(), "@beta", "beta"));
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.request_id, second.request_id);
        assert_eq!(result.prefix, "@beta");
        assert!(result.values.iter().any(|value| value.path == "beta.rs"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalidation_finds_a_new_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("first.rs"), "first").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut service = FileSearchService::new(tx);

        service.search(query(temp.path(), "@first", "first"));
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        std::fs::write(temp.path().join("second.rs"), "second").unwrap();
        service.invalidate_all();
        service.search(query(temp.path(), "@second", "second"));
        let result = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(result.values.iter().any(|value| value.path == "second.rs"));
    }
}
