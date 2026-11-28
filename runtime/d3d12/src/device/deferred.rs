//! Graphics pipelines released after they compile and rebuilt when first bound.
//!
//! A title can create far more pipelines while loading than it ever draws with.
//! ProjectScarlett creates over 80,000, and the NVIDIA driver keeps about
//! 150 KB of host memory for each one that stays alive. Past a first few
//! thousand, a pipeline is compiled as before, so the driver's cache holds it,
//! then released. What is needed to create it again is kept, and the first
//! bind does so from that cache.

use super::{Device, compute::PipelineData, late_pipeline::Resolved, stream};
use core::{ffi::c_void, ptr};
use std::collections::HashSet;
use std::ffi::CStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{IUnknown, Interface, PCSTR};

/// Graphics pipelines that stay resident before later ones are released.
const RESIDENT: usize = 4096;

/// `TUXBOX_RESIDENT_PIPELINES`: how many stay resident. `0` releases every one
/// after the first; `all` keeps them all.
static LIMIT: LazyLock<usize> = LazyLock::new(|| {
    parse_limit(std::env::var("TUXBOX_RESIDENT_PIPELINES").ok().as_deref())
});

fn parse_limit(value: Option<&str>) -> usize {
    match value.map(str::trim) {
        Some("all") => usize::MAX,
        value => value.and_then(|text| text.parse().ok()).unwrap_or(RESIDENT),
    }
}

/// One device's record of the pipelines it has released.
#[derive(Default)]
pub(super) struct Deferred {
    created: AtomicUsize,
    /// The host object every released pipeline's proxy holds. It is never
    /// bound: binding one rebuilds it first.
    placeholder: OnceLock<ID3D12PipelineState>,
    /// Shader bytes and semantic names, kept once however many streams use them.
    shared: Mutex<HashSet<Arc<[u8]>>>,
}

impl Deferred {
    fn share(&self, bytes: &[u8]) -> Arc<[u8]> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = shared.get(bytes) {
            return held.clone();
        }
        let held: Arc<[u8]> = bytes.into();
        shared.insert(held.clone());
        held
    }
}

/// A copy of a title's pipeline stream that owns everything it points at.
pub(super) struct Recipe {
    stream: Box<[usize]>,
    size: usize,
    _root: Option<IUnknown>,
    _shared: Vec<Arc<[u8]>>,
    _layout: Box<[D3D12_INPUT_ELEMENT_DESC]>,
}

impl Recipe {
    /// Copy a stream the title can still change or free. `None` for a stream
    /// that cannot be kept: a compute program, stream output, view instancing,
    /// or a subobject this does not know.
    ///
    /// # Safety
    /// `desc` and everything it points at must be readable, and its root
    /// signature handle must be null or a live COM object.
    pub(super) unsafe fn capture(
        desc: &D3D12_PIPELINE_STATE_STREAM_DESC,
        mut share: impl FnMut(&[u8]) -> Arc<[u8]>,
    ) -> Option<Self> {
        let size = desc.SizeInBytes;
        let input = desc.pPipelineStateSubobjectStream.cast::<u8>();
        if input.is_null() || size == 0 || size > 4096 || !size.is_multiple_of(size_of::<usize>()) {
            return None;
        }
        let mut copy = vec![0usize; size / size_of::<usize>()].into_boxed_slice();
        let output = copy.as_mut_ptr().cast::<u8>();
        unsafe { ptr::copy_nonoverlapping(input, output, size) };
        let mut root = None;
        let mut shared = Vec::new();
        let mut layout: Box<[D3D12_INPUT_ELEMENT_DESC]> = Box::default();
        let mut at = 0;
        while at < size {
            let record = unsafe { output.add(at) };
            let tag = unsafe { record.cast::<i32>().read() };
            let (offset, bytes) = stream::layout(tag)?;
            if bytes > size - at {
                return None;
            }
            let payload = unsafe { record.add(offset) };
            match tag {
                0 => {
                    let handle = unsafe { payload.cast::<*mut c_void>().read() };
                    root = unsafe { IUnknown::from_raw_borrowed(&handle) }.cloned();
                }
                tag if stream::is_shader(tag) => {
                    let shader = unsafe { &mut *payload.cast::<D3D12_SHADER_BYTECODE>() };
                    if shader.BytecodeLength != 0 {
                        // The compute stage compiles on its own deadline.
                        if tag == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS.0
                            || shader.pShaderBytecode.is_null()
                            || shader.BytecodeLength > 64 << 20
                        {
                            return None;
                        }
                        let code = share(unsafe {
                            core::slice::from_raw_parts(shader.pShaderBytecode.cast(), shader.BytecodeLength)
                        });
                        shader.pShaderBytecode = code.as_ptr().cast();
                        shared.push(code);
                    }
                }
                tag if tag == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_STREAM_OUTPUT.0 => {
                    if unsafe { (*payload.cast::<D3D12_STREAM_OUTPUT_DESC>()).NumEntries } != 0 {
                        return None;
                    }
                    unsafe { payload.cast::<D3D12_STREAM_OUTPUT_DESC>().write(Default::default()) };
                }
                tag if tag == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING.0 => {
                    if unsafe { (*payload.cast::<D3D12_VIEW_INSTANCING_DESC>()).ViewInstanceCount } != 0 {
                        return None;
                    }
                    unsafe { payload.cast::<D3D12_VIEW_INSTANCING_DESC>().write(Default::default()) };
                }
                tag if tag == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT.0 => {
                    let guest = unsafe { &mut *payload.cast::<D3D12_INPUT_LAYOUT_DESC>() };
                    let count = guest.NumElements as usize;
                    if count != 0 {
                        if guest.pInputElementDescs.is_null() || count > 64 {
                            return None;
                        }
                        let mut elements: Box<[D3D12_INPUT_ELEMENT_DESC]> =
                            unsafe { core::slice::from_raw_parts(guest.pInputElementDescs, count) }.into();
                        for element in &mut elements {
                            if element.SemanticName.is_null() {
                                return None;
                            }
                            let name = unsafe { CStr::from_ptr(element.SemanticName.0.cast()) };
                            let name = share(name.to_bytes_with_nul());
                            element.SemanticName = PCSTR(name.as_ptr());
                            shared.push(name);
                        }
                        guest.pInputElementDescs = elements.as_ptr();
                        layout = elements;
                    }
                }
                // The console's cached blob is only a hint, and is not used.
                tag if tag == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO.0 => unsafe {
                    payload.cast::<D3D12_CACHED_PIPELINE_STATE>().write(Default::default())
                },
                // Every other known subobject holds values only.
                _ => {}
            }
            at += bytes;
        }
        Some(Self { stream: copy, size, _root: root, _shared: shared, _layout: layout })
    }

    pub(super) fn desc(&self) -> D3D12_PIPELINE_STATE_STREAM_DESC {
        D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: self.size,
            pPipelineStateSubobjectStream: self.stream.as_ptr().cast_mut().cast(),
        }
    }
}

type Built = (ID3D12PipelineState, Box<PipelineData>);

/// A released pipeline.
pub(crate) struct Lazy {
    recipe: Recipe,
    /// The device that owns the pipeline's proxy, which keeps it alive.
    device: *const Device,
    built: OnceLock<Option<Built>>,
}

// SAFETY: the stream and everything it points at are owned and never written
// after capture (creation works on its own copy), the root is a free-threaded
// COM object, and the device is free-threaded and outlives the pipeline.
unsafe impl Send for Lazy {}
unsafe impl Sync for Lazy {}

impl Lazy {
    /// The pipeline and its bindings, created now if this is its first bind.
    /// `released` is what the proxy holds, returned only if creation fails.
    pub(super) fn resolve<'a>(&'a self, released: Resolved<'a>) -> Resolved<'a> {
        let built = self.built.get_or_init(|| {
            // SAFETY: see the Send and Sync implementations.
            let device = unsafe { &*self.device };
            crate::failures::begin();
            let mut data = Box::<PipelineData>::default();
            // SAFETY: the recipe owns everything its stream points at.
            match unsafe { stream::create(device, &self.recipe.desc(), &mut data) } {
                Ok(native) => {
                    static REBUILT: AtomicUsize = AtomicUsize::new(0);
                    let count = REBUILT.fetch_add(1, Ordering::Relaxed) + 1;
                    if count.is_power_of_two() {
                        crate::diagnostics::log(&format!("rebuilt a released graphics pipeline on first use ({count} so far)"));
                    }
                    Some((native, data))
                }
                Err(error) => {
                    crate::diagnostics::log(&format!(
                        "a released graphics pipeline could not be rebuilt: {:#010x}", error.code().0
                    ));
                    None
                }
            }
        });
        match built {
            Some((native, data)) => (native, data),
            None => released,
        }
    }
}

/// Keep `native` for the title's proxy, or release it and keep a recipe.
///
/// # Safety
/// As `Recipe::capture`, for the stream `native` was just created from.
pub(super) unsafe fn release(
    device: &Device,
    desc: &D3D12_PIPELINE_STATE_STREAM_DESC,
    native: ID3D12PipelineState,
    data: PipelineData,
) -> (ID3D12PipelineState, PipelineData) {
    let deferred = &device.state.deferred;
    if deferred.created.fetch_add(1, Ordering::Relaxed) < *LIMIT {
        return (native, data);
    }
    let Some(recipe) = (unsafe { Recipe::capture(desc, |bytes| deferred.share(bytes)) }) else {
        return (native, data);
    };
    static FIRST: std::sync::Once = std::sync::Once::new();
    crate::diagnostics::log_once(&FIRST, || {
        format!("graphics pipelines past the first {} are released until first use", *LIMIT)
    });
    let placeholder = deferred.placeholder.get_or_init(|| native.clone()).clone();
    drop(native);
    let lazy = Lazy { recipe, device: ptr::from_ref(device), built: OnceLock::new() };
    let mut released = PipelineData::default();
    released.lazy = Some(Box::new(lazy));
    (placeholder, released)
}

#[cfg(test)]
#[path = "../tests/device/deferred.rs"]
mod tests;
