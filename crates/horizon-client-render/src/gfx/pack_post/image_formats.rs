//! Image formats: a storage image's format qualifier as wgpu's, and the stand-in a module's
//! storage images declare.

/// Makes each storage image of `module` in group 0 declare the format its layout entry binds:
/// a `layout(r11f_g11f_b10f)` image whose texture is the `RGBA16F` stand-in (`bindings`).
pub(super) fn storage_formats_of(
    module: &mut naga::Module,
    entries: &[wgpu::BindGroupLayoutEntry],
) {
    let globals: Vec<_> = module.global_variables.iter().map(|(h, _)| h).collect();
    for handle in globals {
        let var = &module.global_variables[handle];
        let Some(naga::ResourceBinding { group: 0, binding }) = var.binding else {
            continue;
        };
        let naga::TypeInner::Image {
            dim,
            arrayed,
            class: naga::ImageClass::Storage { format, access },
        } = module.types[var.ty].inner
        else {
            continue;
        };
        let bound = entries.iter().find_map(|e| match e.ty {
            wgpu::BindingType::StorageTexture { format, .. } if e.binding == binding => {
                Some(format)
            }
            _ => None,
        });
        if format == naga::StorageFormat::Rg11b10Ufloat
            && bound == Some(wgpu::TextureFormat::Rgba16Float)
        {
            let ty = module.types.insert(
                naga::Type {
                    name: module.types[var.ty].name.clone(),
                    inner: naga::TypeInner::Image {
                        dim,
                        arrayed,
                        class: naga::ImageClass::Storage {
                            format: naga::StorageFormat::Rgba16Float,
                            access,
                        },
                    },
                },
                naga::Span::UNDEFINED,
            );
            module.global_variables[handle].ty = ty;
        }
    }
}
