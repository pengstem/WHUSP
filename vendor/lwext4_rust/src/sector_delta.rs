/// Describes whether a transaction sector was applied to its original image
/// or merged with a disjoint update published after that image was read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectorDeltaMerge {
    Exact,
    Merged,
}

/// Applies only the bytes changed by `desired` relative to `base` onto
/// `current`.
///
/// This is a three-way merge for metadata sectors. Bytes untouched by the
/// transaction always retain their current value. A changed byte is accepted
/// only when the current image still contains the base value (or already
/// contains the desired value); a third value is a real conflict.
pub fn merge_sector_delta(
    base: &[u8],
    desired: &[u8],
    current: &mut [u8],
) -> Result<SectorDeltaMerge, ()> {
    if base.len() != desired.len() || base.len() != current.len() {
        return Err(());
    }
    let disposition = if current == base {
        SectorDeltaMerge::Exact
    } else {
        SectorDeltaMerge::Merged
    };

    for ((&base_byte, &desired_byte), &current_byte) in base.iter().zip(desired).zip(current.iter())
    {
        if desired_byte != base_byte && current_byte != base_byte && current_byte != desired_byte {
            return Err(());
        }
    }
    for ((&base_byte, &desired_byte), current_byte) in
        base.iter().zip(desired).zip(current.iter_mut())
    {
        if desired_byte != base_byte {
            *current_byte = desired_byte;
        }
    }
    Ok(disposition)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    #[test]
    fn exact_base_applies_transaction_delta() {
        let base = vec![0u8; 16];
        let mut desired = base.clone();
        desired[3] = 7;
        let mut current = base.clone();
        assert_eq!(
            merge_sector_delta(&base, &desired, &mut current),
            Ok(SectorDeltaMerge::Exact)
        );
        assert_eq!(current, desired);
    }

    #[test]
    fn disjoint_update_is_preserved() {
        let base = vec![0u8; 16];
        let mut desired = base.clone();
        desired[3] = 7;
        let mut current = base.clone();
        current[12] = 9;
        assert_eq!(
            merge_sector_delta(&base, &desired, &mut current),
            Ok(SectorDeltaMerge::Merged)
        );
        assert_eq!(current[3], 7);
        assert_eq!(current[12], 9);
    }

    #[test]
    fn same_byte_third_value_conflicts_without_mutation() {
        let base = vec![0u8; 16];
        let mut desired = base.clone();
        desired[3] = 7;
        let mut current = base.clone();
        current[3] = 8;
        let before = current.clone();
        assert_eq!(merge_sector_delta(&base, &desired, &mut current), Err(()));
        assert_eq!(current, before);
    }

    #[test]
    fn already_desired_byte_is_idempotent() {
        let base = vec![0u8; 16];
        let mut desired = base.clone();
        desired[3] = 7;
        let mut current = base.clone();
        current[3] = 7;
        assert_eq!(
            merge_sector_delta(&base, &desired, &mut current),
            Ok(SectorDeltaMerge::Merged)
        );
        assert_eq!(current, desired);
    }

    #[test]
    fn length_mismatch_is_rejected() {
        let base = [0u8; 2];
        let desired = [0u8; 3];
        let mut current = [0u8; 2];
        assert_eq!(merge_sector_delta(&base, &desired, &mut current), Err(()));
    }
}
