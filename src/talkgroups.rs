//! Talkgroup collection and selection.

use std::collections::hash_map::HashMap;
use std::collections::HashSet;

use fnv::FnvBuildHasher;
use p25::voice::crypto::CryptoAlgorithm;

/// Maps talkgroups to associated encryption algorithm.
pub type GroupCryptoMap = HashMap<u16, CryptoAlgorithm, FnvBuildHasher>;

/// Collects, prioritizes, filters, and selects talkgroups.
#[derive(Default)]
pub struct TalkgroupSelection {
    /// Current set of candidate talkgroups.
    cur: Vec<u16>,
    /// Subset of `cur` talkgroups that can preempt a conversation.
    cur_preempt: Vec<u16>,
    /// Channel frequency associated with each candidate talkgroup.
    channels: HashMap<u16, u32, FnvBuildHasher>,
    /// Set of talkgroups that have been observed to be encrypted.
    encrypted: GroupCryptoMap,
    /// Set of talkgroups that can preempt a conversation.
    preempt: HashSet<u16, FnvBuildHasher>,
    /// User-set included/excluded talkgroups.
    filter: Filter,
    /// Talkgroup selection features.
    feats: TalkgroupFeatures,
}

impl TalkgroupSelection {
    /// Record the given elapsed amount of baseband samples.
    pub fn record_elapsed(&mut self, samples: usize) {
        self.feats.record_elapsed(samples);
    }

    /// Consider the given talkgroup for the current set of candidate talkgroups.
    pub fn add_talkgroup(&mut self, tg: u16, freq: u32) {
        if self.encrypted.contains_key(&tg) || self.filter.excluded(tg) {
            return;
        }

        self.feats.add(tg);

        if self.channels.insert(tg, freq).is_some() {
            return;
        }

        debug!("collecting talkgroup {}", tg);

        self.cur.push(tg);

        if self.preempt.contains(&tg) {
            self.cur_preempt.push(tg);
        }
    }

    /// Select a talkgroup from the set of candidate non-preempting talkgroups.
    ///
    /// If a talkgroup is available, return `Some((tg, freq))`, where `tg` is the
    /// talkgroup ID and `freq` is the traffic channel center frequency (Hz). Otherwise,
    /// return `None` if no talkgroups are available.
    pub fn select_idle(&mut self) -> Option<(u16, u32)> {
        debug!("selecting from {} talkgroups", self.cur.len());
        self.feats.max_score(&self.cur).map(|tg| self.select_tg(tg))
    }

    /// Select a talkgroup from the set of candidate preempting talkgroups.
    ///
    /// If a talkgroup is available, return `Some((tg, freq))`, where `tg` is the
    /// talkgroup ID and `freq` is the traffic channel center frequency (Hz). Otherwise,
    /// return `None` if no talkgroups are available.
    pub fn select_preempt(&mut self) -> Option<(u16, u32)> {
        self.feats.max_score(&self.cur_preempt).map(|tg| self.select_tg(tg))
    }

    /// Record that the given talkgroup is encrypted.
    pub fn record_encrypted(&mut self, tg: u16, alg: CryptoAlgorithm) {
        debug!("marking talkgroup {} as encrypted with {:?}", tg, alg);
        self.encrypted.insert(tg, alg);
    }

    /// Finalize selection of the given talkgroup.
    fn select_tg(&mut self, tg: u16) -> (u16, u32) {
        debug!("using talkgroup {}", tg);

        let freq = self.channels[&tg];

        self.clear_candidates();
        self.feats.select(tg);

        (tg, freq)
    }

    /// Clear candidate talkgroup state.
    fn clear_candidates(&mut self) {
        self.cur.clear();
        self.cur_preempt.clear();
        self.channels.clear();
    }

    /// Clear state related to the current talkgroup site.
    pub fn clear_state(&mut self) {
        self.clear_candidates();
        self.encrypted.clear();
        self.feats.reset();
    }
}

/// Tracks features used to score and rank talkgroups.
///
/// The weighted-sum method is used to compute a score for each talkgroup.
#[derive(Default)]
struct TalkgroupFeatures {
    /// Current baseband sample counter since the last talkgroup selection.
    elapsed: usize,
    /// Timestamp when each talkgroup in `cur` was added.
    age: HashMap<u16, usize, FnvBuildHasher>,
    /// Set of recently-visited talkgroups and associated visit timestamp.
    recent: u16,
    /// User-set talkgroup priorities.
    pub prios: HashMap<u16, f32, FnvBuildHasher>,
    /// User-set weights for each feature used when scoring each talkgroup.
    pub weights: FeatureWeights,
}

impl TalkgroupFeatures {
    /// Record elapsed baseband samples.
    pub fn record_elapsed(&mut self, samples: usize) {
        self.elapsed = self.elapsed.wrapping_add(samples);
    }

    /// Add the given talkgroup to the set of candidates (or update its age.)
    pub fn add(&mut self, tg: u16) {
        self.age.insert(tg, self.elapsed);
    }

    /// Update state to reflect that the given talkgroup was selected.
    pub fn select(&mut self, tg: u16) {
        self.age.clear();
        self.recent = tg;
        self.elapsed = 0;
    }

    /// Reset talkgroup-related state.
    pub fn reset(&mut self) {
        self.select(0);
    }

    /// Retrieve the oldest age over all talkgroups.
    fn oldest(&self) -> usize {
        self.elapsed.wrapping_sub(self.age.values().cloned().min().unwrap_or(0))
    }

    /// Find the talkgroup with the highest score in the given candidate talkgroups.
    ///
    /// Each talkgroup must have been previously recorded with the `add` method.
    pub fn max_score(&self, groups: &[u16]) -> Option<u16> {
        let oldest = self.oldest() as f32;

        // If the oldest talkgroup has no age, then none of the others will either, so
        // just set the multiplier to zero to avoid divide-by-zero.
        let mul = if oldest == 0.0 { 0.0 } else { oldest.recip() };

        let score = |tg| {
            // Older talkgroups score lower.
            let age = 1.0 - self.elapsed.wrapping_sub(self.age[&tg]) as f32 * mul;
            // Recent talkgroup gets a reward.
            let recent = if tg == self.recent { 1.0 } else { 0.0 };

            self.prios.get(&tg).unwrap_or(&1.0) * self.weights.prio +
            age * self.weights.age +
            recent * self.weights.recent
        };

        groups.iter().cloned()
              .max_by(|&a, &b| score(a).partial_cmp(&score(b)).unwrap())
    }
}

/// Weights for features used in talkgroup selection.
#[derive(Serialize, Deserialize)]
pub struct FeatureWeights {
    /// Weight of user priority.
    prio: f32,
    /// Weight of talkgroup age.
    age: f32,
    /// Weight of recently-selected talkgroup reward.
    recent: f32,
}

impl Default for FeatureWeights {
    fn default() -> Self {
        FeatureWeights {
            prio: 1.0,
            age: 1.0,
            recent: 1.0,
        }
    }
}

/// Filters talkgroups with an include-by-default or exclude-by-default policy.
#[derive(Serialize, Deserialize)]
pub struct Filter {
    /// Whether the talkgroups in `tg` should be excluded (include-by-default) or included
    /// (exclude-by-default).
    exclude: bool,
    /// Included/excluded talkgroups.
    filt: HashSet<u16, FnvBuildHasher>,
}

impl Default for Filter {
    /// Create a new `Filter` in an empty include-by-default state.
    fn default() -> Self {
        Filter {
            exclude: true,
            filt: HashSet::default(),
        }
    }
}

impl Filter {
    /// Check if the given talkgroup is excluded from selection.
    pub fn excluded(&self, tg: u16) -> bool {
        let filtered = self.filt.contains(&tg);
        self.exclude && filtered || !self.exclude && !filtered
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_filter() {
        use serde_json;

        let mut f = Filter::default();
        assert!(!f.excluded(42));
        f.exclude = false;
        assert!(f.excluded(42));
        f.filt.insert(42);
        assert!(!f.excluded(42));
        f.exclude = true;
        assert!(f.excluded(42));
        assert!(!f.excluded(43));

        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(j, "{\"exclude\":true,\"filt\":[42]}");

        let f: Filter = serde_json::from_str("{\"filt\": [1, 2, 3], \"exclude\": false}")
                            .unwrap();
        assert!(!f.exclude);
        assert_eq!(f.filt.len(), 3);
        assert!(f.filt.contains(&1));
        assert!(f.filt.contains(&2));
        assert!(f.filt.contains(&3));
    }

    #[test]
    fn test_age() {
        let mut ts = TalkgroupSelection::default();
        ts.add_talkgroup(11, 1);
        ts.record_elapsed(10);
        assert_eq!(ts.feats.oldest(), 10);
        ts.add_talkgroup(22, 2);
        ts.record_elapsed(20);
        assert_eq!(ts.feats.oldest(), 30);
        ts.add_talkgroup(33, 3);
        ts.record_elapsed(30);
        assert_eq!(ts.feats.oldest(), 60);
        ts.add_talkgroup(44, 4);
        ts.record_elapsed(40);
        assert_eq!(ts.feats.oldest(), 100);
        assert_eq!(ts.select_idle(), Some((44, 4)));

        ts.clear_state();

        ts.add_talkgroup(11, 1);
        ts.record_elapsed(10);
        assert_eq!(ts.feats.oldest(), 10);
        ts.add_talkgroup(22, 2);
        ts.record_elapsed(20);
        assert_eq!(ts.feats.oldest(), 30);
        ts.add_talkgroup(33, 3);
        ts.record_elapsed(30);
        assert_eq!(ts.feats.oldest(), 60);
        ts.add_talkgroup(44, 4);
        ts.record_elapsed(40);
        assert_eq!(ts.feats.oldest(), 100);
        ts.feats.weights.age = -1.0;
        assert_eq!(ts.select_idle(), Some((11, 1)));
    }

    #[test]
    fn test_selection() {
        let mut ts = TalkgroupSelection::default();
        assert_eq!(ts.select_idle(), None);

        // Test initial select.
        ts.add_talkgroup(10, 42);
        assert_eq!(&ts.cur[..], &[10]);
        assert!(ts.cur_preempt.is_empty());
        assert_eq!(ts.select_preempt(), None);
        assert_eq!(ts.select_idle(), Some((10, 42)));
        assert_eq!(ts.feats.recent, 10);
        assert_eq!(ts.select_idle(), None);
        assert_eq!(ts.feats.recent, 10);
        assert!(ts.feats.age.is_empty());
        assert!(ts.channels.is_empty());

        // Test preempt talkgroup.
        ts.preempt.insert(20);
        ts.add_talkgroup(10, 800);
        assert_eq!(&ts.cur[..], &[10]);
        assert!(ts.cur_preempt.is_empty());
        ts.add_talkgroup(20, 200);
        assert_eq!(&ts.cur[..], &[10, 20]);
        assert_eq!(&ts.cur_preempt[..], &[20]);
        assert_eq!(ts.feats.recent, 10);
        assert_eq!(ts.select_preempt(), Some((20, 200)));
        assert_eq!(ts.feats.recent, 20);
        assert_eq!(ts.select_preempt(), None);
        assert_eq!(ts.select_idle(), None);
        assert!(ts.feats.age.is_empty());
        assert!(ts.channels.is_empty());

        // Test encrypted filter.
        ts.encrypted.insert(20, CryptoAlgorithm::Aes);
        ts.add_talkgroup(20, 12);
        assert!(ts.cur.is_empty());
        assert!(ts.cur_preempt.is_empty());

        // Test user filter.
        ts.filter.filt.insert(30);
        ts.add_talkgroup(30, 22);
        assert!(ts.cur.is_empty());

        // Test user priority.
        ts.feats.prios.insert(40, 100.0);
        ts.add_talkgroup(10, 100);
        ts.add_talkgroup(20, 200);
        ts.add_talkgroup(30, 300);
        ts.add_talkgroup(40, 400);
        ts.add_talkgroup(50, 500);
        assert_eq!(&ts.cur[..], &[10, 40, 50]);
        assert!(ts.cur_preempt.is_empty());
        assert_eq!(ts.select_preempt(), None);
        assert_eq!(ts.select_idle(), Some((40, 400)));
        assert_eq!(ts.feats.recent, 40);
        assert_eq!(ts.select_idle(), None);
        assert!(ts.feats.age.is_empty());
        assert!(ts.channels.is_empty());

        ts.clear_state();
        assert!(ts.encrypted.is_empty());
        assert_eq!(ts.feats.recent, 0);
    }
}
