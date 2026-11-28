//! Compute pipelines whose driver compile outlives the pipeline deadline.
//!
//! The title is given an empty stand-in so it can keep loading, and the compile
//! carries on. Without a way back, the stand-in stayed for the whole session:
//! on a cold driver cache ProjectScarlett received hundreds, most of them at once
//! because every compile slot was taken. The finished compile now fills the
//! stand-in's slot, and the next bind uses the real pipeline.
//!
//! Dispatches recorded before then are still skipped.
use super::compute::{Pipeline, PipelineData};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use windows::Win32::Graphics::Direct3D12::ID3D12PipelineState;

/// Where a running compile leaves its pipeline.
pub(crate) struct Slot<N>(Arc<(OnceLock<N>, AtomicBool)>);

impl<N> Clone for Slot<N> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<N> Default for Slot<N> {
    fn default() -> Self {
        Self(Arc::new((OnceLock::new(), AtomicBool::new(false))))
    }
}

impl<N> Slot<N> {
    pub(crate) fn fill(&self, pipeline: N) {
        let _ = self.0.0.set(pipeline);
    }

    /// The compiled pipeline with its bindings once it exists, otherwise the
    /// stand-in with its own. Both come from one look, so they always match.
    /// The flag is true the first time the compiled one is returned.
    pub(crate) fn pick<'a, D>(&'a self, standin: (&'a N, &'a D), compiled: &'a D) -> (&'a N, &'a D, bool) {
        match self.0.0.get() {
            Some(pipeline) => (pipeline, compiled, !self.0.1.swap(true, Ordering::Relaxed)),
            None => (standin.0, standin.1, false),
        }
    }
}

/// What a stand-in needs to become the pipeline the title asked for.
pub(crate) struct Late {
    pub(crate) slot: Slot<ID3D12PipelineState>,
    /// The bindings of the compiled pipeline. The stand-in has none of them.
    pub(crate) data: PipelineData,
}

/// A guest pipeline as it binds now. Resolve it once per bind, so the host call
/// and the recorded bindings agree.
pub(crate) type Resolved<'a> = (&'a ID3D12PipelineState, &'a PipelineData);

/// The pipeline to bind for `pipeline` now, with the bindings that go with it.
pub(crate) fn current(pipeline: &Pipeline) -> Resolved<'_> {
    if let Some(lazy) = &pipeline.state.lazy {
        return lazy.resolve((&pipeline.native, &pipeline.state));
    }
    let Some(late) = &pipeline.state.late else { return (&pipeline.native, &pipeline.state) };
    let (native, data, first) = late.slot.pick((&pipeline.native, &pipeline.state), &late.data);
    if first {
        static REPLACED: AtomicUsize = AtomicUsize::new(0);
        let count = REPLACED.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            crate::diagnostics::log(&format!("a finished compile replaced its stand-in compute pipeline ({count} so far)"));
        }
    }
    (native, data)
}

#[cfg(test)]
#[path = "../tests/device/late_pipeline.rs"]
mod tests;
