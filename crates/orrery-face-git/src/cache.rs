//! Answers the GitHub face works out from git, kept in process memory and
//! looked up by the commits they were worked out from.
//!
//! A merge of two commits, a comparison of two, and one commit's date depend
//! on nothing but the commits, and a commit never changes, so an answer kept
//! here is never stale and is never invalidated. Two inputs git reads beside
//! the commits are not looked at again once an answer is kept: a replace ref,
//! which a client can push to `refs/replace/` to make git read another commit
//! in a commit's place, and, on a git that reads a bare repository's
//! attributes from the tree `HEAD` names, as git 2.43 does, a `.gitattributes`
//! pushed to the default branch, which can mark a file binary.
//!
//! What is kept is derived from the world, not part of it: it lives in no
//! file, so a snapshot never copies it and a restart works it out again. An
//! answer is kept only when it was worked out from full object ids, since a
//! name such as a branch can come to mean another commit, and only when git
//! answered it, since a failure such as a missing object can be followed by
//! a push that supplies it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use orrery_world::Repo;

use crate::store::{self, ChangedFile, DiffStats, FileDiff};
use crate::{GitFace, GitFaceError};

/// The most memory each kind of answer is kept in, in bytes, as
/// [`Lru::insert`] counts them: 56 MiB in all, which a world reaches only
/// once its reads have covered tens of thousands of comparisons. A pull
/// request's test merge, its diff totals, and its head's date each cost about
/// a third of a KiB, and its changed files about 0.1 KiB more per file.
const MERGES_BYTES: usize = 8 << 20;
const DIFF_STATS_BYTES: usize = 8 << 20;
const COMMIT_DATES_BYTES: usize = 8 << 20;
const CHANGED_FILES_BYTES: usize = 16 << 20;
const FILE_DIFFS_BYTES: usize = 16 << 20;

/// What an entry costs beside its key's strings and its value's: the key's
/// copy in the recency order, and both maps' bookkeeping.
const ENTRY_BYTES: usize = 256;

/// A comparison in one repository: its repository's id, then the base and
/// head commits.
type PairKey = (i64, String, String);

pub(crate) struct Answers {
    merges: Mutex<Lru<PairKey, bool>>,
    diff_stats: Mutex<Lru<PairKey, DiffStats>>,
    changed_files: Mutex<Lru<PairKey, Arc<[ChangedFile]>>>,
    file_diffs: Mutex<Lru<PairKey, Arc<[FileDiff]>>>,
    commit_dates: Mutex<Lru<(i64, String), Option<i64>>>,
}

impl Answers {
    pub(crate) fn new() -> Answers {
        Answers {
            merges: Mutex::new(Lru::new(MERGES_BYTES)),
            diff_stats: Mutex::new(Lru::new(DIFF_STATS_BYTES)),
            changed_files: Mutex::new(Lru::new(CHANGED_FILES_BYTES)),
            file_diffs: Mutex::new(Lru::new(FILE_DIFFS_BYTES)),
            commit_dates: Mutex::new(Lru::new(COMMIT_DATES_BYTES)),
        }
    }
}

impl GitFace {
    /// Whether the merge of each `(base, head)` is clean, as
    /// [`store::merge_tree`] finds it: from what is kept, and the rest from
    /// one batch ([`store::merges_clean`]).
    pub fn merges_clean(
        &self,
        repo: &Repo,
        pairs: &[(&str, &str)],
    ) -> Vec<Result<bool, GitFaceError>> {
        let bare = self.bare(repo);
        through(
            &self.answers.merges,
            pairs,
            |pair| pair_key(repo, pair),
            |_| Some(0),
            |missed| store::merges_clean(&bare, missed),
        )
    }

    /// [`store::diff_stats`] for each `(base, head)`: from what is kept, and
    /// the rest from one batch ([`store::diff_stats_many`]).
    pub fn diff_stats(
        &self,
        repo: &Repo,
        pairs: &[(&str, &str)],
    ) -> Vec<Result<DiffStats, GitFaceError>> {
        let bare = self.bare(repo);
        through(
            &self.answers.diff_stats,
            pairs,
            |pair| pair_key(repo, pair),
            |_| Some(0),
            |missed| store::diff_stats_many(&bare, missed),
        )
    }

    /// [`store::changed_files`] for each `(base, head)`: from what is kept,
    /// and the rest from one batch ([`store::changed_files_many`]).
    pub fn changed_files(
        &self,
        repo: &Repo,
        pairs: &[(&str, &str)],
    ) -> Vec<Result<Arc<[ChangedFile]>, GitFaceError>> {
        let bare = self.bare(repo);
        through(
            &self.answers.changed_files,
            pairs,
            |pair| pair_key(repo, pair),
            |files: &Arc<[ChangedFile]>| Some(files.iter().map(file_bytes).sum()),
            |missed| {
                store::changed_files_many(&bare, missed)
                    .into_iter()
                    .map(|files| files.map(Arc::from))
                    .collect()
            },
        )
    }

    /// [`store::file_diffs`] of `base` and `head`, from what is kept when it
    /// can be.
    pub fn file_diffs(
        &self,
        repo: &Repo,
        base: &str,
        head: &str,
    ) -> Result<Arc<[FileDiff]>, GitFaceError> {
        let bare = self.bare(repo);
        through(
            &self.answers.file_diffs,
            &[(base, head)],
            |pair| pair_key(repo, pair),
            |diffs: &Arc<[FileDiff]>| {
                Some(
                    diffs
                        .iter()
                        .map(|d| file_bytes(&d.file) + d.patch.as_ref().map_or(0, String::len))
                        .sum(),
                )
            },
            |missed| {
                missed
                    .iter()
                    .map(|(b, h)| store::file_diffs(&bare, b, h).map(Arc::from))
                    .collect()
            },
        )
        .pop()
        .expect("one answer for one pair")
    }

    /// [`store::commit_date_unix`] for each of `shas`: from what is kept, and
    /// the rest from one batch ([`store::commit_dates_unix`]). A sha that
    /// names no commit now can name one after a push, so that answer is not
    /// kept.
    pub fn commit_dates(
        &self,
        repo: &Repo,
        shas: &[&str],
    ) -> Vec<Result<Option<i64>, GitFaceError>> {
        let bare = self.bare(repo);
        through(
            &self.answers.commit_dates,
            shas,
            |sha| store::is_oid(sha).then(|| (repo.id, sha.to_string())),
            |date: &Option<i64>| date.map(|_| 0),
            |missed| store::commit_dates_unix(&bare, missed),
        )
    }

    fn bare(&self, repo: &Repo) -> std::path::PathBuf {
        store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name)
    }
}

/// The key a comparison is kept under; `None` when a side is not a full
/// object id.
fn pair_key(repo: &Repo, (base, head): &(&str, &str)) -> Option<PairKey> {
    (store::is_oid(base) && store::is_oid(head))
        .then(|| (repo.id, base.to_string(), head.to_string()))
}

/// What a changed file costs beside its entry: its paths and the struct.
fn file_bytes(file: &ChangedFile) -> usize {
    file.path.len() + file.previous.as_ref().map_or(0, String::len) + 64
}

/// A key's strings, which [`Lru::insert`] is told the cost of.
trait KeyBytes {
    fn key_bytes(&self) -> usize;
}

impl KeyBytes for PairKey {
    fn key_bytes(&self) -> usize {
        self.1.len() + self.2.len()
    }
}

impl KeyBytes for (i64, String) {
    fn key_bytes(&self) -> usize {
        self.1.len()
    }
}

/// The answer to each question: the kept ones from `cache`, and the rest
/// from one call of `compute` with every question missed, in order. An
/// answer `compute` gets right is kept when `weigh` gives what it costs
/// beside its key, and `key` gives a key for its question.
fn through<Q: Copy, K: Ord + Clone + KeyBytes, V: Clone>(
    cache: &Mutex<Lru<K, V>>,
    questions: &[Q],
    key: impl Fn(&Q) -> Option<K>,
    weigh: impl Fn(&V) -> Option<usize>,
    compute: impl FnOnce(&[Q]) -> Vec<Result<V, GitFaceError>>,
) -> Vec<Result<V, GitFaceError>> {
    let keys: Vec<Option<K>> = questions.iter().map(&key).collect();
    let mut answers: Vec<Option<Result<V, GitFaceError>>> = {
        let mut kept = cache.lock().expect("cache mutex poisoned");
        keys.iter()
            .map(|k| k.as_ref().and_then(|k| kept.get(k)).map(Ok))
            .collect()
    };
    let missed: Vec<Q> = questions
        .iter()
        .zip(&answers)
        .filter(|(_, a)| a.is_none())
        .map(|(q, _)| *q)
        .collect();
    if !missed.is_empty() {
        let mut found = compute(&missed).into_iter();
        let mut kept = cache.lock().expect("cache mutex poisoned");
        for (k, answer) in keys.into_iter().zip(&mut answers) {
            if answer.is_some() {
                continue;
            }
            let value = found.next().expect("one answer per missed question");
            if let (Some(k), Ok(v)) = (k, &value)
                && let Some(bytes) = weigh(v)
            {
                let bytes = bytes + k.key_bytes();
                kept.insert(k, v.clone(), bytes);
            }
            *answer = Some(value);
        }
    }
    answers.into_iter().flatten().collect()
}

/// A map holding entries up to a budget of bytes, dropping the one used
/// longest ago to make room. Ordered maps throughout, so nothing here
/// depends on hash order.
struct Lru<K, V> {
    entries: BTreeMap<K, Entry<V>>,
    /// Each entry's key by when it was last used, oldest first.
    recency: BTreeMap<u64, K>,
    clock: u64,
    bytes: usize,
    budget: usize,
}

struct Entry<V> {
    value: V,
    used: u64,
    bytes: usize,
}

impl<K: Ord + Clone, V: Clone> Lru<K, V> {
    fn new(budget: usize) -> Lru<K, V> {
        Lru {
            entries: BTreeMap::new(),
            recency: BTreeMap::new(),
            clock: 0,
            bytes: 0,
            budget,
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.get_mut(key)?;
        self.recency.remove(&entry.used);
        self.clock += 1;
        entry.used = self.clock;
        self.recency.insert(self.clock, key.clone());
        Some(entry.value.clone())
    }

    /// Keep `value` under `key`, at `bytes` plus [`ENTRY_BYTES`]. One that
    /// alone is over the budget is not kept.
    fn insert(&mut self, key: K, value: V, bytes: usize) {
        let bytes = bytes + ENTRY_BYTES;
        if bytes > self.budget {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.recency.remove(&old.used);
            self.bytes -= old.bytes;
        }
        while self.bytes + bytes > self.budget {
            let Some((_, oldest)) = self.recency.pop_first() else {
                break;
            };
            if let Some(dropped) = self.entries.remove(&oldest) {
                self.bytes -= dropped.bytes;
            }
        }
        self.clock += 1;
        self.recency.insert(self.clock, key.clone());
        self.entries.insert(
            key,
            Entry {
                value,
                used: self.clock,
                bytes,
            },
        );
        self.bytes += bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_the_entry_used_longest_ago() {
        let mut lru: Lru<u32, u32> = Lru::new(3 * (ENTRY_BYTES + 1));
        for k in 0..3 {
            lru.insert(k, k * 10, 1);
        }
        assert_eq!(lru.get(&0), Some(0));
        lru.insert(3, 30, 1);
        assert_eq!(lru.get(&1), None, "1 was used longest ago");
        assert_eq!(lru.get(&0), Some(0));
        assert_eq!(lru.get(&2), Some(20));
        assert_eq!(lru.get(&3), Some(30));
        assert_eq!(lru.bytes, 3 * (ENTRY_BYTES + 1));
        assert_eq!(lru.entries.len(), lru.recency.len());
    }

    #[test]
    fn an_entry_over_the_budget_is_not_kept() {
        let mut lru: Lru<u32, u32> = Lru::new(2 * ENTRY_BYTES);
        lru.insert(1, 1, 0);
        lru.insert(2, 2, 2 * ENTRY_BYTES);
        assert_eq!(lru.get(&2), None);
        assert_eq!(lru.get(&1), Some(1), "a refused entry evicts nothing");
    }

    #[test]
    fn replacing_an_entry_frees_what_it_held() {
        let mut lru: Lru<u32, u32> = Lru::new(2 * (ENTRY_BYTES + 10));
        lru.insert(1, 1, 10);
        lru.insert(1, 2, 10);
        lru.insert(2, 3, 10);
        assert_eq!(lru.get(&1), Some(2));
        assert_eq!(lru.get(&2), Some(3));
        assert_eq!(lru.bytes, 2 * (ENTRY_BYTES + 10));
    }
}
