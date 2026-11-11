//! Which trackers of a torrent are "in use" right now (BEP 12).
//!
//! The rules are a pure function of the availability of every tracker, so that all the
//! combinations of the two announce options can be tested without any I/O.

/// What the scheduler knows about one tracker, as far as choosing it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    /// Never tried, or failed and the retry is due: it must be tried next.
    Pending,
    /// The last announce worked.
    Working,
    /// Failed, and is backing off: not to be used until the retry is due.
    Waiting,
    /// Unusable for good (unsupported scheme, panicked).
    Disabled,
}

impl Availability {
    fn usable(self) -> bool {
        matches!(self, Availability::Pending | Availability::Working)
    }
}

/// How announces are spread over the tiers and the trackers of a tier. Mirrors libtorrent's
/// `announce_to_all_tiers` / `announce_to_all_trackers`. The default is to announce to
/// everything, which is what rqbit has always done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackerAnnounceOptions {
    /// Announce to a tracker of every tier. When false, only the first tier that has a usable
    /// tracker is announced to, and the next tiers are tried only when it stops working.
    pub announce_to_all_tiers: bool,
    /// Announce to every tracker of a tier. When false, only the first usable tracker of the
    /// tier is announced to, and the next ones are tried only when it stops working.
    pub announce_to_all_trackers: bool,
}

impl Default for TrackerAnnounceOptions {
    fn default() -> Self {
        Self {
            announce_to_all_tiers: true,
            announce_to_all_trackers: true,
        }
    }
}

/// Returns `(tier, position)` of every tracker that is in use. `tiers[t][p]` is the
/// availability of the tracker at position `p` of tier `t`, in the order the tier is kept
/// (the tracker that answered last is first).
pub(crate) fn in_use(
    tiers: &[Vec<Availability>],
    options: TrackerAnnounceOptions,
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (t, tier) in tiers.iter().enumerate() {
        let mut any = false;
        for (p, a) in tier.iter().enumerate() {
            if !a.usable() {
                continue;
            }
            out.push((t, p));
            any = true;
            if !options.announce_to_all_trackers {
                break;
            }
        }
        if any && !options.announce_to_all_tiers {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Availability::*;
    use super::*;

    fn opts(tiers: bool, trackers: bool) -> TrackerAnnounceOptions {
        TrackerAnnounceOptions {
            announce_to_all_tiers: tiers,
            announce_to_all_trackers: trackers,
        }
    }

    fn fixture() -> Vec<Vec<Availability>> {
        vec![
            vec![Waiting, Pending, Working],
            vec![Working, Pending],
            vec![Pending],
        ]
    }

    #[test]
    fn everything_by_default() {
        assert_eq!(TrackerAnnounceOptions::default(), opts(true, true));
        assert_eq!(
            in_use(&fixture(), opts(true, true)),
            vec![(0, 1), (0, 2), (1, 0), (1, 1), (2, 0)]
        );
    }

    #[test]
    fn one_tracker_per_tier() {
        assert_eq!(
            in_use(&fixture(), opts(true, false)),
            vec![(0, 1), (1, 0), (2, 0)]
        );
    }

    #[test]
    fn all_trackers_of_the_first_working_tier() {
        assert_eq!(in_use(&fixture(), opts(false, true)), vec![(0, 1), (0, 2)]);
        // The first tier is all backing off: the second one takes over.
        let tiers = vec![
            vec![Waiting, Disabled],
            vec![Working, Pending],
            vec![Pending],
        ];
        assert_eq!(in_use(&tiers, opts(false, true)), vec![(1, 0), (1, 1)]);
    }

    #[test]
    fn plain_bep12() {
        assert_eq!(in_use(&fixture(), opts(false, false)), vec![(0, 1)]);
        // Fail over inside the tier, then to the next tier.
        let tiers = vec![
            vec![Waiting, Waiting],
            vec![Waiting, Pending],
            vec![Pending],
        ];
        assert_eq!(in_use(&tiers, opts(false, false)), vec![(1, 1)]);
        let tiers = vec![vec![Waiting, Disabled], vec![Waiting], vec![Pending]];
        assert_eq!(in_use(&tiers, opts(false, false)), vec![(2, 0)]);
        // Nothing usable: nothing in use.
        let tiers = vec![vec![Waiting], vec![Disabled]];
        assert!(in_use(&tiers, opts(false, false)).is_empty());
    }
}
