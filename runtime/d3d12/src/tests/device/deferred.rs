use super::*;

/// Records laid out as the title writes them: a tag, then the payload at its
/// own alignment, padded to a pointer.
#[repr(C, align(8))]
struct Record<T> {
    tag: i32,
    value: T,
}

#[repr(C)]
struct Stream {
    vertex: Record<D3D12_SHADER_BYTECODE>,
    layout: Record<D3D12_INPUT_LAYOUT_DESC>,
    mask: Record<u32>,
}

fn stream(code: &[u8], elements: &[D3D12_INPUT_ELEMENT_DESC]) -> Stream {
    Stream {
        vertex: Record {
            tag: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS.0,
            value: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
        },
        layout: Record {
            tag: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT.0,
            value: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: elements.as_ptr(), NumElements: elements.len() as u32 },
        },
        mask: Record { tag: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK.0, value: 0x1234 },
    }
}

fn desc(stream: &Stream) -> D3D12_PIPELINE_STATE_STREAM_DESC {
    D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<Stream>(),
        pPipelineStateSubobjectStream: ptr::from_ref(stream).cast_mut().cast(),
    }
}

#[test]
fn a_recipe_still_reads_the_same_after_the_title_frees_its_stream() {
    let deferred = Deferred::default();
    let recipes: Vec<_> = (0..2)
        .map(|_| {
            let mut code = vec![7u8; 64];
            let mut name = b"TEXCOORD\0".to_vec();
            let mut elements = vec![D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr()),
                SemanticIndex: 3,
                ..Default::default()
            }];
            let guest = stream(&code, &elements);
            let recipe = unsafe { Recipe::capture(&desc(&guest), |bytes| deferred.share(bytes)) }.unwrap();
            code.fill(0);
            name.fill(b'x');
            elements[0].SemanticIndex = 9;
            recipe
        })
        .collect();
    for recipe in &recipes {
        let copy = unsafe { &*recipe.desc().pPipelineStateSubobjectStream.cast::<Stream>() };
        let code = unsafe {
            core::slice::from_raw_parts(copy.vertex.value.pShaderBytecode.cast::<u8>(), copy.vertex.value.BytecodeLength)
        };
        assert_eq!(code, [7u8; 64]);
        let element = unsafe { &*copy.layout.value.pInputElementDescs };
        assert_eq!(unsafe { CStr::from_ptr(element.SemanticName.0.cast()) }, c"TEXCOORD");
        assert_eq!((element.SemanticIndex, copy.mask.value), (3, 0x1234));
    }
    // The shader and the name are held once for both.
    assert_eq!(deferred.shared.lock().unwrap().len(), 2);
}

#[test]
fn a_compute_stream_is_not_released() {
    let code = [1u8; 32];
    let mut guest = stream(&code, &[]);
    guest.vertex.tag = D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS.0;
    assert!(unsafe { Recipe::capture(&desc(&guest), |bytes| bytes.into()) }.is_none());
}

#[test]
fn the_resident_limit_reads_a_count_or_all() {
    assert_eq!(parse_limit(None), RESIDENT);
    assert_eq!(parse_limit(Some("0")), 0);
    assert_eq!(parse_limit(Some("all")), usize::MAX);
    assert_eq!(parse_limit(Some("many")), RESIDENT);
}
