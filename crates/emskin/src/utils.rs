use smithay::utils::{Coordinate, Size};

pub(crate) trait SizeExt<N: Coordinate, Kind> {
    fn at_least(self, min: impl Into<Size<N, Kind>>) -> Size<N, Kind>;
}

impl<N: Coordinate, Kind> SizeExt<N, Kind> for Size<N, Kind> {
    fn at_least(self, min: impl Into<Size<N, Kind>>) -> Size<N, Kind> {
        let min = min.into();
        (self.w.max(min.w), self.h.max(min.h)).into()
    }
}
