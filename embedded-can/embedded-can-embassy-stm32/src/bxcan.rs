use embedded_can::{ExtendedId, StandardId};
use embedded_can_interface::{FilterConfig, IdMask, IdMaskFilter};

use embassy_stm32::can;

use crate::{EmbassyStm32CanError, EmbassyStm32Rx, EmbassyStm32Tx};

/// Filter configuration support for embassy-stm32 BxCAN targets.
///
/// Enable the crate feature `bxcan` to compile this support.
impl<'d> FilterConfig for EmbassyStm32Rx<'d> {
    type Error = EmbassyStm32CanError;
    type FiltersHandle<'a>
        = can::filter::MasterFilters<'a>
    where
        Self: 'a;

    fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), Self::Error> {
        use can::filter::{Mask16, Mask32};

        let mut banks = self.as_inner_mut().modify_filters();
        let bank_limit = banks.num_banks() as usize;

        if filters.is_empty() {
            banks.clear();
            banks.enable_bank(0, can::Fifo::Fifo0, Mask32::accept_all());
            return Ok(());
        }

        let mut required = 0usize;
        let mut pending_std = false;
        for f in filters {
            match (f.id, f.mask) {
                (embedded_can_interface::Id::Standard(_), IdMask::Standard(mask_raw)) => {
                    if StandardId::new(mask_raw).is_none() {
                        return Err(EmbassyStm32CanError::MaskOutOfRange);
                    }
                    if pending_std {
                        required += 1;
                        pending_std = false;
                    } else {
                        pending_std = true;
                    }
                }
                (embedded_can_interface::Id::Extended(_), IdMask::Extended(mask_raw)) => {
                    if ExtendedId::new(mask_raw).is_none() {
                        return Err(EmbassyStm32CanError::MaskOutOfRange);
                    }
                    if pending_std {
                        required += 1;
                        pending_std = false;
                    }
                    required += 1;
                }
                _ => return Err(EmbassyStm32CanError::MaskWidthMismatch),
            }
        }
        if pending_std {
            required += 1;
        }
        if required > bank_limit {
            return Err(EmbassyStm32CanError::TooManyFilters);
        }

        banks.clear();
        let mut bank = 0u8;

        let mut pending_std: Option<(StandardId, StandardId)> = None;
        for f in filters {
            match (f.id, f.mask) {
                (embedded_can_interface::Id::Standard(id), IdMask::Standard(mask_raw)) => {
                    let mask =
                        StandardId::new(mask_raw).ok_or(EmbassyStm32CanError::MaskOutOfRange)?;
                    if let Some((id2, mask2)) = pending_std.take() {
                        banks.enable_bank(
                            bank,
                            can::Fifo::Fifo0,
                            [
                                Mask16::frames_with_std_id(id2, mask2),
                                Mask16::frames_with_std_id(id, mask),
                            ],
                        );
                        bank += 1;
                    } else {
                        pending_std = Some((id, mask));
                    }
                }
                (embedded_can_interface::Id::Extended(id), IdMask::Extended(mask_raw)) => {
                    let mask =
                        ExtendedId::new(mask_raw).ok_or(EmbassyStm32CanError::MaskOutOfRange)?;
                    if let Some((std_id, std_mask)) = pending_std.take() {
                        banks.enable_bank(
                            bank,
                            can::Fifo::Fifo0,
                            Mask32::frames_with_std_id(std_id, std_mask),
                        );
                        bank += 1;
                    }
                    banks.enable_bank(bank, can::Fifo::Fifo0, Mask32::frames_with_ext_id(id, mask));
                    bank += 1;
                }
                _ => return Err(EmbassyStm32CanError::MaskWidthMismatch),
            }
        }
        if let Some((id, mask)) = pending_std.take() {
            banks.enable_bank(bank, can::Fifo::Fifo0, Mask32::frames_with_std_id(id, mask));
        }

        Ok(())
    }

    fn modify_filters(&mut self) -> Self::FiltersHandle<'_> {
        self.as_inner_mut().modify_filters()
    }
}

/// Convenience helper for BxCAN targets: wrap the `embassy-stm32` split halves.
///
/// Note: On BxCAN, dropping the original `embassy_stm32::can::Can` resets the peripheral, so keep
/// it alive for at least as long as these halves are used.
pub fn wrap_split_bxcan<'d>(can: &mut can::Can<'d>) -> (EmbassyStm32Tx<'d>, EmbassyStm32Rx<'d>) {
    let (tx, rx) = can.split();
    (EmbassyStm32Tx::new(tx), EmbassyStm32Rx::new(rx))
}
