//! # wgsl_bindgen
//! wgsl_bindgen is an experimental library for generating typesafe Rust bindings from WGSL shaders to [wgpu](https://github.com/gfx-rs/wgpu).
//!
//! ## Features
//! The `WgslBindgenOptionBuilder` is used to configure the generation of Rust bindings from WGSL shaders. This facilitates a shader focused workflow where edits to WGSL code are automatically reflected in the corresponding Rust file. For example, changing the type of a uniform in WGSL will raise a compile error in Rust code using the generated struct to initialize the buffer.
//!
//! Writing Rust code to interact with WGSL shaders can be tedious and error prone, especially when the types and functions in the shader code change during development. wgsl_bindgen is not a rendering library and does not offer high level abstractions like a scene graph or material system. However, using generated code still has a number of advantages compared to writing the code by hand.
//!
//! The code generated by wgsl_bindgen can help with valid API usage like:
//! - setting all bind groups and bind group bindings
//! - setting correct struct fields and field types for vertex input buffers
//! - setting correct struct struct fields and field types for storage and uniform buffers
//! - configuring shader initialization
//! - getting vertex attribute offsets for vertex buffers
//! - const validation of struct memory layouts when using bytemuck
//!
//! Here's an example of how to use `WgslBindgenOptionBuilder` to generate Rust bindings from WGSL shaders:
//!
//! ```no_run
//! use miette::{IntoDiagnostic, Result};
//! use wgsl_bindgen::{WgslTypeSerializeStrategy, WgslBindgenOptionBuilder, GlamWgslTypeMap};
//!
//! fn main() -> Result<()> {
//!     WgslBindgenOptionBuilder::default()
//!         .workspace_root("src/shader")
//!         .add_entry_point("src/shader/testbed.wgsl")
//!         .add_entry_point("src/shader/triangle.wgsl")
//!         .skip_hash_check(true)
//!         .serialization_strategy(WgslTypeSerializeStrategy::Bytemuck)
//!         .type_map(GlamWgslTypeMap)
//!         .derive_serde(false)
//!         .output("src/shader.rs".to_string())
//!         .build()?
//!         .generate()
//!         .into_diagnostic()
//! }
//! ```

#[allow(dead_code, unused)]
extern crate wgpu_types as wgpu;

use bevy_util::SourceWithFullDependenciesResult;
use case::CaseExt;
use derive_more::IsVariant;
use generate::{bind_group, consts, pipeline, shader_module, shader_registry};
use heck::ToPascalCase;
use naga::ShaderStage;
use proc_macro2::{Literal, Span, TokenStream};
use qs::{format_ident, quote, Ident, Index};
use quote_gen::{custom_vector_matrix_assertions, RustModBuilder, MOD_STRUCT_ASSERTIONS};
use thiserror::Error;

pub mod bevy_util;
mod bindgen;
mod generate;
mod naga_util;
mod quote_gen;
mod structs;
mod types;
mod wgsl;
mod wgsl_type;

pub mod qs {
  pub use proc_macro2::TokenStream;
  pub use quote::{format_ident, quote};
  pub use syn::{Ident, Index};
}

pub use bindgen::*;
pub use naga::FastIndexMap;
pub use regex::Regex;
pub use types::*;
pub use wgsl_type::*;

/// Enum representing the possible serialization strategies for WGSL types.
///
/// This enum is used to specify how WGSL types should be serialized when converted
/// to Rust types.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, IsVariant)]
pub enum WgslTypeSerializeStrategy {
  #[default]
  Encase,
  Bytemuck,
}

/// Errors while generating Rust source for a WGSl shader module.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum CreateModuleError {
  /// Bind group sets must be consecutive and start from 0.
  /// See `bind_group_layouts` for
  /// [PipelineLayoutDescriptor](https://docs.rs/wgpu/latest/wgpu/struct.PipelineLayoutDescriptor.html#).
  #[error("bind groups are non-consecutive or do not start from 0")]
  NonConsecutiveBindGroups,

  /// Each binding resource must be associated with exactly one binding index.
  #[error("duplicate binding found with index `{binding}`")]
  DuplicateBinding { binding: u32 },
}

pub(crate) struct WgslEntryResult<'a> {
  mod_name: String,
  naga_module: naga::Module,
  source_including_deps: SourceWithFullDependenciesResult<'a>,
}

fn create_rust_bindings(
  entries: Vec<WgslEntryResult<'_>>,
  options: &WgslBindgenOption,
) -> Result<String, CreateModuleError> {
  let mut mod_builder = RustModBuilder::new(true);

  if let Some(custom_wgsl_type_asserts) = custom_vector_matrix_assertions(options) {
    mod_builder.add(MOD_STRUCT_ASSERTIONS, custom_wgsl_type_asserts);
  }

  for entry in entries.iter() {
    let WgslEntryResult {
      mod_name,
      naga_module,
      ..
    } = entry;
    let entry_name = sanitize_and_pascal_case(&entry.mod_name);
    let bind_group_data = bind_group::get_bind_group_data(naga_module)?;
    let shader_stages = wgsl::shader_stages(naga_module);

    // Write all the structs, including uniforms and entry function inputs.
    mod_builder
      .add_items(structs::structs_items(&mod_name, naga_module, options))
      .unwrap();

    mod_builder
      .add_items(consts::consts_items(&mod_name, naga_module))
      .unwrap();

    mod_builder.add(mod_name, vertex_struct_methods(naga_module));

    mod_builder.add(
      mod_name,
      bind_group::bind_groups_module(
        &mod_name,
        &options,
        &bind_group_data,
        shader_stages,
      ),
    );

    mod_builder.add(
      mod_name,
      shader_module::compute_module(naga_module, options.shader_source_type),
    );
    mod_builder.add(mod_name, entry_point_constants(naga_module));
    mod_builder.add(mod_name, vertex_states(naga_module));

    let create_pipeline_layout =
      pipeline::create_pipeline_layout_fn(&entry_name, &options, &bind_group_data);
    mod_builder.add(mod_name, create_pipeline_layout);
    mod_builder.add(mod_name, shader_module::shader_module(entry, options));
  }

  let mod_token_stream = mod_builder.generate();
  let shader_registry =
    shader_registry::build_shader_registry(&entries, options.shader_source_type);

  let output = quote! {
    #![allow(unused, non_snake_case, non_camel_case_types, non_upper_case_globals)]

    #shader_registry
    #mod_token_stream
  };

  Ok(pretty_print(&output))
}

fn pretty_print(tokens: &TokenStream) -> String {
  let file = syn::parse_file(&tokens.to_string()).unwrap();
  prettyplease::unparse(&file)
}

fn indexed_name_ident(name: &str, index: u32) -> Ident {
  format_ident!("{name}{index}")
}

fn sanitize_and_pascal_case(v: &str) -> String {
  v.chars()
    .filter(|ch| ch.is_alphanumeric() || *ch == '_')
    .collect::<String>()
    .to_pascal_case()
}

fn sanitized_upper_snake_case(v: &str) -> String {
  v.chars()
    .filter(|ch| ch.is_alphanumeric() || *ch == '_')
    .collect::<String>()
    .to_snake()
    .to_uppercase()
}

fn vertex_struct_methods(module: &naga::Module) -> TokenStream {
  let structs = vertex_input_structs(module);
  quote!(#(#structs)*)
}

fn entry_point_constants(module: &naga::Module) -> TokenStream {
  let entry_points: Vec<TokenStream> = module
    .entry_points
    .iter()
    .map(|entry_point| {
      let entry_name = Literal::string(&entry_point.name);
      let const_name = Ident::new(
        &format!("ENTRY_{}", &entry_point.name.to_uppercase()),
        Span::call_site(),
      );
      quote! {
          pub const #const_name: &str = #entry_name;
      }
    })
    .collect();

  quote! {
      #(#entry_points)*
  }
}

fn vertex_states(module: &naga::Module) -> TokenStream {
  let vertex_inputs = wgsl::get_vertex_input_structs(module);
  let mut step_mode_params = vec![];
  let layout_expressions: Vec<TokenStream> = vertex_inputs
    .iter()
    .map(|input| {
      let name = Ident::new(&input.name, Span::call_site());
      let step_mode = Ident::new(&input.name.to_snake(), Span::call_site());
      step_mode_params.push(quote!(#step_mode: wgpu::VertexStepMode));
      quote!(#name::vertex_buffer_layout(#step_mode))
    })
    .collect();

  let vertex_entries: Vec<TokenStream> = module
    .entry_points
    .iter()
    .filter_map(|entry_point| match &entry_point.stage {
      ShaderStage::Vertex => {
        let fn_name =
          Ident::new(&format!("{}_entry", &entry_point.name), Span::call_site());
        let const_name = Ident::new(
          &format!("ENTRY_{}", &entry_point.name.to_uppercase()),
          Span::call_site(),
        );
        let n = vertex_inputs.len();
        let n = Literal::usize_unsuffixed(n);
        Some(quote! {
            pub fn #fn_name(#(#step_mode_params),*) -> VertexEntry<#n> {
                VertexEntry {
                    entry_point: #const_name,
                    buffers: [
                        #(#layout_expressions),*
                    ]
                }
            }
        })
      }
      _ => None,
    })
    .collect();

  // Don't generate unused code.
  if vertex_entries.is_empty() {
    quote!()
  } else {
    quote! {
        #[derive(Debug)]
        pub struct VertexEntry<const N: usize> {
            entry_point: &'static str,
            buffers: [wgpu::VertexBufferLayout<'static>; N]
        }

        pub fn vertex_state<'a, const N: usize>(
            module: &'a wgpu::ShaderModule,
            entry: &'a VertexEntry<N>,
        ) -> wgpu::VertexState<'a> {
            wgpu::VertexState {
                module,
                entry_point: entry.entry_point,
                buffers: &entry.buffers,
                compilation_options: Default::default(),
            }
        }

        #(#vertex_entries)*
    }
  }
}

fn vertex_input_structs(module: &naga::Module) -> Vec<TokenStream> {
  let vertex_inputs = wgsl::get_vertex_input_structs(module);
  vertex_inputs.iter().map(|input|  {
        let name = Ident::new(&input.name, Span::call_site());

        // Use index to avoid adding prefix to literals.
        let count = Index::from(input.fields.len());
        let attributes: Vec<_> = input
            .fields
            .iter()
            .map(|(location, m)| {
                let field_name: TokenStream = m.name.as_ref().unwrap().parse().unwrap();
                let location = Index::from(*location as usize);
                let format = wgsl::vertex_format(&module.types[m.ty]);
                // TODO: Will the debug implementation always work with the macro?
                let format = Ident::new(&format!("{format:?}"), Span::call_site());

                quote! {
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::#format,
                        offset: std::mem::offset_of!(#name, #field_name) as u64,
                        shader_location: #location,
                    }
                }
            })
            .collect();


        // The vertex_attr_array! macro doesn't account for field alignment.
        // Structs with glam::Vec4 and glam::Vec3 fields will not be tightly packed.
        // Manually calculate the Rust field offsets to support using bytemuck for vertices.
        // This works since we explicitly mark all generated structs as repr(C).
        // Assume elements are in Rust arrays or slices, so use size_of for stride.
        // TODO: Should this enforce WebGPU alignment requirements for compatibility?
        // https://gpuweb.github.io/gpuweb/#abstract-opdef-validating-gpuvertexbufferlayout

        // TODO: Support vertex inputs that aren't in a struct.
        quote! {
            impl #name {
                pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; #count] = [#(#attributes),*];

                pub const fn vertex_buffer_layout(step_mode: wgpu::VertexStepMode) -> wgpu::VertexBufferLayout<'static> {
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<#name>() as u64,
                        step_mode,
                        attributes: &#name::VERTEX_ATTRIBUTES
                    }
                }
            }
        }
    }).collect()
}

// Tokenstreams can't be compared directly using PartialEq.
// Use pretty_print to normalize the formatting and compare strings.
// Use a colored diff output to make differences easier to see.
#[cfg(test)]
#[macro_export]
macro_rules! assert_tokens_eq {
  ($a:expr, $b:expr) => {
    pretty_assertions::assert_eq!(crate::pretty_print(&$a), crate::pretty_print(&$b))
  };
}

#[cfg(test)]
mod test {
  use indoc::indoc;

  use self::bevy_util::source_file::SourceFile;
  use super::*;

  fn create_shader_module(
    source: &str,
    options: WgslBindgenOption,
  ) -> Result<String, CreateModuleError> {
    let naga_module = naga::front::wgsl::parse_str(source).unwrap();
    let dummy_source = SourceFile::create(SourceFilePath::new(""), None, "".into());
    let entry = WgslEntryResult {
      mod_name: "test".into(),
      naga_module,
      source_including_deps: SourceWithFullDependenciesResult {
        full_dependencies: Default::default(),
        source_file: &dummy_source,
      },
    };

    Ok(create_rust_bindings(vec![entry], &options)?)
  }

  #[test]
  fn create_shader_module_embed_source() {
    let source = indoc! {r#"
            @fragment
            fn fs_main() {}
        "#};

    let actual = create_shader_module(source, WgslBindgenOption::default()).unwrap();

    pretty_assertions::assert_eq!(
      indoc! {r##"
                #![allow(unused, non_snake_case, non_camel_case_types, non_upper_case_globals)]
                #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
                pub enum ShaderEntry {
                    Test,
                }
                impl ShaderEntry {
                    pub fn create_pipeline_layout(&self, device: &wgpu::Device) -> wgpu::PipelineLayout {
                        match self {
                            Self::Test => test::create_pipeline_layout(device),
                        }
                    }
                    pub fn create_shader_module_embed_source(
                        &self,
                        device: &wgpu::Device,
                    ) -> wgpu::ShaderModule {
                        match self {
                            Self::Test => test::create_shader_module_embed_source(device),
                        }
                    }
                }
                mod _root {
                    pub use super::*;
                }
                pub mod test {
                    use super::{_root, _root::*};
                    pub const ENTRY_FS_MAIN: &str = "fs_main";
                    #[derive(Debug)]
                    pub struct WgpuPipelineLayout;
                    impl WgpuPipelineLayout {
                        pub fn bind_group_layout_entries(
                            entries: [wgpu::BindGroupLayout; 0],
                        ) -> [wgpu::BindGroupLayout; 0] {
                            entries
                        }
                    }
                    pub fn create_pipeline_layout(device: &wgpu::Device) -> wgpu::PipelineLayout {
                        device
                            .create_pipeline_layout(
                                &wgpu::PipelineLayoutDescriptor {
                                    label: Some("Test::PipelineLayout"),
                                    bind_group_layouts: &[],
                                    push_constant_ranges: &[],
                                },
                            )
                    }
                    pub fn create_shader_module_embed_source(
                        device: &wgpu::Device,
                    ) -> wgpu::ShaderModule {
                        let source = std::borrow::Cow::Borrowed(SHADER_STRING);
                        device
                            .create_shader_module(wgpu::ShaderModuleDescriptor {
                                label: None,
                                source: wgpu::ShaderSource::Wgsl(source),
                            })
                    }
                    pub const SHADER_STRING: &'static str = r#"
                @fragment 
                fn fs_main() {
                    return;
                }
                "#;
                }
            "##},
      actual
    );
  }

  #[test]
  fn create_shader_module_consecutive_bind_groups() {
    let source = indoc! {r#"
            struct A {
                f: vec4<f32>
            };
            @group(0) @binding(0) var<uniform> a: A;
            @group(1) @binding(0) var<uniform> b: A;

            @vertex
            fn vs_main() -> @builtin(position) vec4<f32> {
              return vec4<f32>(0.0, 0.0, 0.0, 1.0);
            }

            @fragment
            fn fs_main() {}
        "#};

    create_shader_module(source, WgslBindgenOption::default()).unwrap();
  }

  #[test]
  fn create_shader_module_non_consecutive_bind_groups() {
    let source = indoc! {r#"
            @group(0) @binding(0) var<uniform> a: vec4<f32>;
            @group(1) @binding(0) var<uniform> b: vec4<f32>;
            @group(3) @binding(0) var<uniform> c: vec4<f32>;

            @fragment
            fn main() {}
        "#};

    let result = create_shader_module(source, WgslBindgenOption::default());
    assert!(matches!(result, Err(CreateModuleError::NonConsecutiveBindGroups)));
  }

  #[test]
  fn create_shader_module_repeated_bindings() {
    let source = indoc! {r#"
            struct A {
                f: vec4<f32>
            };
            @group(0) @binding(2) var<uniform> a: A;
            @group(0) @binding(2) var<uniform> b: A;

            @fragment
            fn main() {}
        "#};

    let result = create_shader_module(source, WgslBindgenOption::default());
    assert!(matches!(result, Err(CreateModuleError::DuplicateBinding { binding: 2 })));
  }

  #[test]
  fn write_vertex_module_empty() {
    let source = indoc! {r#"
            @vertex
            fn main() {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(quote!(), actual);
  }

  #[test]
  fn write_vertex_module_single_input_float32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: f32,
                @location(1) b: vec2<f32>,
                @location(2) c: vec3<f32>,
                @location(3) d: vec4<f32>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float32x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_float64() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: f64,
                @location(1) b: vec2<f64>,
                @location(2) c: vec3<f64>,
                @location(3) d: vec4<f64>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Float64x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_sint32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: i32,
                @location(1) a: vec2<i32>,
                @location(2) a: vec3<i32>,
                @location(3) a: vec4<i32>,

            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x2,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x3,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Sint32x4,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_vertex_module_single_input_uint32() {
    let source = indoc! {r#"
            struct VertexInput0 {
                @location(0) a: u32,
                @location(1) b: vec2<u32>,
                @location(2) c: vec3<u32>,
                @location(3) d: vec4<u32>,
            };

            @vertex
            fn main(in0: VertexInput0) {}
        "#};

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_struct_methods(&module);

    assert_tokens_eq!(
      quote! {
          impl VertexInput0 {
              pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = [
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32,
                      offset: std::mem::offset_of!(VertexInput0, a) as u64,
                      shader_location: 0,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x2,
                      offset: std::mem::offset_of!(VertexInput0, b) as u64,
                      shader_location: 1,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x3,
                      offset: std::mem::offset_of!(VertexInput0, c) as u64,
                      shader_location: 2,
                  },
                  wgpu::VertexAttribute {
                      format: wgpu::VertexFormat::Uint32x4,
                      offset: std::mem::offset_of!(VertexInput0, d) as u64,
                      shader_location: 3,
                  },
              ];
              pub const fn vertex_buffer_layout(
                  step_mode: wgpu::VertexStepMode,
              ) -> wgpu::VertexBufferLayout<'static> {
                  wgpu::VertexBufferLayout {
                      array_stride: std::mem::size_of::<VertexInput0>() as u64,
                      step_mode,
                      attributes: &VertexInput0::VERTEX_ATTRIBUTES,
                  }
              }
          }
      },
      actual
    );
  }

  #[test]
  fn write_entry_constants() {
    let source = indoc! {r#"
            @vertex
            fn vs_main() {}

            @vertex
            fn another_vs() {}

            @fragment
            fn fs_main() {}

            @fragment
            fn another_fs() {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = entry_point_constants(&module);

    assert_tokens_eq!(
      quote! {
          pub const ENTRY_VS_MAIN: &str = "vs_main";
          pub const ENTRY_ANOTHER_VS: &str = "another_vs";
          pub const ENTRY_FS_MAIN: &str = "fs_main";
          pub const ENTRY_ANOTHER_FS: &str = "another_fs";
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_entry_no_buffers() {
    let source = indoc! {r#"
            @vertex
            fn vs_main() {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
                  compilation_options: Default::default()
              }
          }
          pub fn vs_main_entry() -> VertexEntry<0> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN,
                  buffers: [],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_multiple_entries() {
    let source = indoc! {r#"
            struct VertexInput {
                @location(0) position: vec4<f32>,
            };
            @vertex
            fn vs_main_1(in: VertexInput) {}

            @vertex
            fn vs_main_2(in: VertexInput) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
                  compilation_options: Default::default(),
              }
          }
          pub fn vs_main_1_entry(vertex_input: wgpu::VertexStepMode) -> VertexEntry<1> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN_1,
                  buffers: [VertexInput::vertex_buffer_layout(vertex_input)],
              }
          }
          pub fn vs_main_2_entry(vertex_input: wgpu::VertexStepMode) -> VertexEntry<1> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN_2,
                  buffers: [VertexInput::vertex_buffer_layout(vertex_input)],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_shader_entry_multiple_buffers() {
    let source = indoc! {r#"
            struct Input0 {
                @location(0) position: vec4<f32>,
            };
            struct Input1 {
                @location(1) some_data: vec2<f32>
            }
            @vertex
            fn vs_main(in0: Input0, in1: Input1) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(
      quote! {
          #[derive(Debug)]
          pub struct VertexEntry<const N: usize> {
              entry_point: &'static str,
              buffers: [wgpu::VertexBufferLayout<'static>; N],
          }
          pub fn vertex_state<'a, const N: usize>(
              module: &'a wgpu::ShaderModule,
              entry: &'a VertexEntry<N>,
          ) -> wgpu::VertexState<'a> {
              wgpu::VertexState {
                  module,
                  entry_point: entry.entry_point,
                  buffers: &entry.buffers,
                  compilation_options: Default::default(),
              }
          }
          pub fn vs_main_entry(input0: wgpu::VertexStepMode, input1: wgpu::VertexStepMode) -> VertexEntry<2> {
              VertexEntry {
                  entry_point: ENTRY_VS_MAIN,
                  buffers: [
                      Input0::vertex_buffer_layout(input0),
                      Input1::vertex_buffer_layout(input1),
                  ],
              }
          }
      },
      actual
    )
  }

  #[test]
  fn write_vertex_states_no_entries() {
    let source = indoc! {r#"
            struct Input {
                @location(0) position: vec4<f32>,
            };
            @fragment
            fn main(in: Input) {}
        "#
    };

    let module = naga::front::wgsl::parse_str(source).unwrap();
    let actual = vertex_states(&module);

    assert_tokens_eq!(quote!(), actual)
  }
}
