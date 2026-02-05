use embedded_can::{ExtendedId, StandardId};
use embedded_can_interface::{FilterConfig, IdMask, IdMaskFilter};

use embassy_stm32::can;

use crate::{EmbassyStm32CanError, EmbassyStm32Rx, EmbassyStm32Tx};

/// FDCAN common properties wrapper (filters, error counters, etc).
#[derive(Debug)]
pub struct EmbassyStm32FdProperties {
    inner: can::Properties,
}

impl EmbassyStm32FdProperties {
    /// Wrap `embassy_stm32::can::Properties`.
    pub fn new(inner: can::Properties) -> Self {
        Self { inner }
    }

    /// Borrow the inner embassy-stm32 properties.
    pub fn as_inner(&self) -> &can::Properties {
        &self.inner
    }

    /// Unwrap into the inner embassy-stm32 properties.
    pub fn into_inner(self) -> can::Properties {
        self.inner
    }
}

impl FilterConfig for EmbassyStm32FdProperties {
    type Error = EmbassyStm32CanError;
    type FiltersHandle<'a>
        = ()
    where
        Self: 'a;

    fn set_filters(&mut self, filters: &[IdMaskFilter]) -> Result<(), Self::Error> {
        use can::filter::{
            Action, EXTENDED_FILTER_MAX, ExtendedFilter, FilterType, STANDARD_FILTER_MAX,
            StandardFilter,
        };

        let mut std = [StandardFilter::disable(); STANDARD_FILTER_MAX as usize];
        let mut ext = [ExtendedFilter::disable(); EXTENDED_FILTER_MAX as usize];

        if filters.is_empty() {
            std[0] = StandardFilter::accept_all_into_fifo0();
            ext[0] = ExtendedFilter::accept_all_into_fifo0();
            self.inner.set_standard_filters(&std);
            self.inner.set_extended_filters(&ext);
            return Ok(());
        }

        let mut std_i = 0usize;
        let mut ext_i = 0usize;

        for f in filters {
            match (f.id, f.mask) {
                (embedded_can_interface::Id::Standard(id), IdMask::Standard(mask)) => {
                    if std_i >= std.len() {
                        return Err(EmbassyStm32CanError::TooManyFilters);
                    }
                    if StandardId::new(mask).is_none() {
                        return Err(EmbassyStm32CanError::MaskOutOfRange);
                    }
                    std[std_i] = StandardFilter {
                        filter: FilterType::BitMask {
                            filter: id.as_raw(),
                            mask,
                        },
                        action: Action::StoreInFifo0,
                    };
                    std_i += 1;
                }
                (embedded_can_interface::Id::Extended(id), IdMask::Extended(mask)) => {
                    if ext_i >= ext.len() {
                        return Err(EmbassyStm32CanError::TooManyFilters);
                    }
                    if ExtendedId::new(mask).is_none() {
                        return Err(EmbassyStm32CanError::MaskOutOfRange);
                    }
                    ext[ext_i] = ExtendedFilter {
                        filter: FilterType::BitMask {
                            filter: id.as_raw(),
                            mask,
                        },
                        action: Action::StoreInFifo0,
                    };
                    ext_i += 1;
                }
                _ => return Err(EmbassyStm32CanError::MaskWidthMismatch),
            }
        }

        self.inner.set_standard_filters(&std);
        self.inner.set_extended_filters(&ext);
        Ok(())
    }

    fn modify_filters(&mut self) -> Self::FiltersHandle<'_> {
        ()
    }
}

/// Convenience helper for FDCAN targets: wrap the `embassy-stm32` split halves.
pub fn wrap_split_fdcan<'d>(
    can: can::Can<'d>,
) -> (
    EmbassyStm32Tx<'d>,
    EmbassyStm32Rx<'d>,
    EmbassyStm32FdProperties,
) {
    let (tx, rx, props) = can.split();
    (
        EmbassyStm32Tx::new(tx),
        EmbassyStm32Rx::new(rx),
        EmbassyStm32FdProperties::new(props),
    )
}
