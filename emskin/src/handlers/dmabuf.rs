use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::ImportDma},
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
};

use crate::EmskinState;

impl DmabufHandler for EmskinState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.wl.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let Some(ref mut backend) = self.backend else {
            tracing::warn!("dmabuf_imported called with no backend");
            notifier.failed();
            return;
        };
        if backend.renderer().import_dmabuf(&dmabuf, None).is_ok() {
            if let Err(e) = notifier.successful::<EmskinState>() {
                tracing::warn!("dmabuf import notification failed: {e:?}");
            }
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(EmskinState);
