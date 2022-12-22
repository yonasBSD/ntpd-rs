use crate::SystemConfig;

use super::{config::AlgorithmConfig, PeerSnapshot};

enum BoundType {
    Start,
    End,
}

pub(super) fn select<Index: Copy>(
    config: &SystemConfig,
    algo_config: &AlgorithmConfig,
    candidates: Vec<PeerSnapshot<Index>>,
) -> Vec<PeerSnapshot<Index>> {
    let mut bounds: Vec<(f64, BoundType)> = Vec::with_capacity(2 * candidates.len());

    for snapshot in candidates.iter() {
        let radius = snapshot.offset_uncertainty() * algo_config.range_statistical_weight
            + snapshot.delay * algo_config.range_delay_weight;
        if radius > algo_config.max_peer_uncertainty || !snapshot.leap_indicator.is_synchronized() {
            continue;
        }

        bounds.push((snapshot.offset() - radius, BoundType::Start));
        bounds.push((snapshot.offset() + radius, BoundType::End));
    }

    bounds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let mut max: usize = 0;
    let mut maxt: f64 = 0.0;
    let mut cur: usize = 0;

    for (time, boundtype) in bounds.iter() {
        match boundtype {
            BoundType::Start => cur += 1,
            BoundType::End => cur -= 1,
        }
        if cur > max {
            max = cur;
            maxt = *time;
        }
    }

    if max >= config.min_intersection_survivors && max * 4 > bounds.len() {
        candidates
            .iter()
            .filter(|snapshot| {
                let radius = snapshot.offset_uncertainty() * algo_config.range_statistical_weight
                    + snapshot.delay * algo_config.range_delay_weight;
                radius <= algo_config.max_peer_uncertainty
                    && snapshot.offset() - radius <= maxt
                    && snapshot.offset() + radius >= maxt
                    && snapshot.leap_indicator.is_synchronized()
            })
            .cloned()
            .collect()
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        algorithm::kalman::{
            matrix::{Matrix, Vector},
            sqr,
        },
        NtpDuration, NtpTimestamp,
    };

    use super::*;

    fn snapshot_for_range(center: f64, uncertainty: f64, delay: f64) -> PeerSnapshot<usize> {
        PeerSnapshot {
            index: 0,
            state: Vector::new(center, 0.0),
            uncertainty: Matrix::new(sqr(uncertainty), 0.0, 0.0, 10e-12),
            delay,
            peer_uncertainty: NtpDuration::from_seconds(0.01),
            peer_delay: NtpDuration::from_seconds(0.01),
            leap_indicator: crate::NtpLeapIndicator::NoWarning,
            last_update: NtpTimestamp::from_fixed_int(0),
        }
    }

    #[test]
    fn test_weighing() {
        let candidates = vec![
            snapshot_for_range(0.0, 0.01, 0.09),
            snapshot_for_range(0.0, 0.09, 0.01),
            snapshot_for_range(0.05, 0.01, 0.09),
            snapshot_for_range(0.05, 0.09, 0.01),
        ];
        let sysconfig = SystemConfig {
            min_intersection_survivors: 4,
            ..Default::default()
        };

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 1.0,
            range_statistical_weight: 1.0,
            range_delay_weight: 0.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 0);

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 1.0,
            range_statistical_weight: 0.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 0);

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 1.0,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_rejection() {
        let candidates = vec![
            snapshot_for_range(0.0, 1.0, 1.0),
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.0, 0.01, 0.01),
        ];
        let sysconfig = SystemConfig {
            min_intersection_survivors: 1,
            ..Default::default()
        };

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 3.0,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 3);

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 0.3,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 2);

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 0.03,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 1);

        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 0.003,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_min_survivors() {
        let candidates = vec![
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.5, 0.1, 0.1),
            snapshot_for_range(0.5, 0.1, 0.1),
        ];
        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 3.0,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };

        let sysconfig = SystemConfig {
            min_intersection_survivors: 3,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates.clone());
        assert_eq!(result.len(), 3);

        let sysconfig = SystemConfig {
            min_intersection_survivors: 4,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_tie() {
        let candidates = vec![
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.0, 0.1, 0.1),
            snapshot_for_range(0.5, 0.1, 0.1),
            snapshot_for_range(0.5, 0.1, 0.1),
        ];
        let algconfig = AlgorithmConfig {
            max_peer_uncertainty: 3.0,
            range_statistical_weight: 1.0,
            range_delay_weight: 1.0,
            ..Default::default()
        };
        let sysconfig = SystemConfig {
            min_intersection_survivors: 1,
            ..Default::default()
        };
        let result = select(&sysconfig, &algconfig, candidates);
        assert_eq!(result.len(), 0);
    }
}