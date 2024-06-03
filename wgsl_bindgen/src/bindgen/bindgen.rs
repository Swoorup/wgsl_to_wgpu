use std::io::Write;

use naga_oil::compose::{
  ComposableModuleDescriptor, Composer, ComposerError, NagaModuleDescriptor,
  ShaderLanguage,
};

use crate::bevy_util::source_file::SourceFile;
use crate::bevy_util::DependencyTree;
use crate::{
  create_rust_bindings, SourceFilePath, SourceWithFullDependenciesResult,
  WgslBindgenError, WgslBindgenOption, WgslEntryResult, WgslShaderIrCapabilities,
};

const PKG_VER: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");

pub struct WGSLBindgen {
  dependency_tree: DependencyTree,
  options: WgslBindgenOption,
  content_hash: String,
}

impl WGSLBindgen {
  pub(crate) fn new(options: WgslBindgenOption) -> Result<Self, WgslBindgenError> {
    let entry_points = options
      .entry_points
      .iter()
      .cloned()
      .map(SourceFilePath::new)
      .collect();

    let dependency_tree = DependencyTree::try_build(
      options.workspace_root.clone(),
      options.module_import_root.clone(),
      entry_points,
      options.additional_scan_dirs.clone(),
    )?;

    let content_hash = Self::get_contents_hash(&options, &dependency_tree);

    if options.emit_rerun_if_change {
      for file in Self::iter_files_to_watch(&dependency_tree) {
        println!("cargo:rerun-if-changed={}", file);
      }
    }

    Ok(Self {
      dependency_tree,
      options,
      content_hash,
    })
  }

  fn iter_files_to_watch(dep_tree: &DependencyTree) -> impl Iterator<Item = String> {
    dep_tree
      .all_files_including_dependencies()
      .into_iter()
      .map(|path| path.to_string())
  }

  fn get_contents_hash(options: &WgslBindgenOption, dep_tree: &DependencyTree) -> String {
    let mut hasher = blake3::Hasher::new();

    hasher.update(format!("{:?}", options).as_bytes());
    hasher.update(PKG_VER.as_bytes());

    for SourceFile { content, .. } in dep_tree.parsed_files() {
      hasher.update(content.as_bytes());
    }

    hasher.finalize().to_string()
  }

  fn generate_naga_module_for_entry(
    ir_capabilities: Option<WgslShaderIrCapabilities>,
    entry: SourceWithFullDependenciesResult<'_>,
  ) -> Result<WgslEntryResult, WgslBindgenError> {
    let map_err = |composer: &Composer, err: ComposerError| {
      let msg = err.emit_to_string(composer);
      WgslBindgenError::NagaModuleComposeError {
        entry: entry.source_file.file_path.to_string(),
        inner: err.inner,
        msg,
      }
    };

    let mut composer = match ir_capabilities {
      Some(WgslShaderIrCapabilities {
        capabilities,
        subgroup_stages,
      }) => Composer::default().with_capabilities(capabilities, subgroup_stages),
      _ => Composer::default(),
    };
    let source = entry.source_file;

    for dependency in entry.full_dependencies.iter() {
      composer
        .add_composable_module(ComposableModuleDescriptor {
          source: &dependency.content,
          file_path: &dependency.file_path.to_string(),
          language: ShaderLanguage::Wgsl,
          as_name: dependency.module_name.as_ref().map(|name| name.to_string()),
          ..Default::default()
        })
        .map(|_| ())
        .map_err(|err| map_err(&composer, err))?;
    }

    let module = composer
      .make_naga_module(NagaModuleDescriptor {
        source: &source.content,
        file_path: &source.file_path.to_string(),
        ..Default::default()
      })
      .map_err(|err| map_err(&composer, err))?;

    Ok(WgslEntryResult {
      mod_name: source.file_path.file_prefix(),
      naga_module: module,
      source_including_deps: entry,
    })
  }

  pub fn header_texts(&self) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    if !self.options.skip_header_comments {
      writeln!(text, "// File automatically generated by {PKG_NAME}^").unwrap();
      writeln!(text, "//").unwrap();
      writeln!(text, "// ^ {PKG_NAME} version {PKG_VER}",).unwrap();
      writeln!(text, "// Changes made to this file will not be saved.").unwrap();
      writeln!(text, "// SourceHash: {}", self.content_hash).unwrap();
      writeln!(text).unwrap();
    }
    text
  }

  fn generate_output(&self) -> Result<String, WgslBindgenError> {
    let ir_capabilities = self.options.ir_capabilities;
    let entry_results = self
      .dependency_tree
      .get_source_files_with_full_dependencies()
      .into_iter()
      .map(|it| Self::generate_naga_module_for_entry(ir_capabilities, it))
      .collect::<Result<Vec<_>, _>>()?;

    Ok(create_rust_bindings(entry_results, &self.options)?)
  }

  pub fn generate_string(&self) -> Result<String, WgslBindgenError> {
    let mut text = self.header_texts();
    text += &self.generate_output()?;
    Ok(text)
  }

  pub fn generate(&self) -> Result<(), WgslBindgenError> {
    let out = self
      .options
      .output
      .as_ref()
      .ok_or(WgslBindgenError::OutputFileNotSpecified)?;

    let old_content = std::fs::read_to_string(out).unwrap_or_else(|_| String::new());

    let old_hashstr_comment = old_content
      .lines()
      .find(|line| line.starts_with("// SourceHash:"))
      .unwrap_or("");

    let is_hash_changed =
      || old_hashstr_comment != format!("// SourceHash: {}", &self.content_hash);

    if self.options.skip_hash_check || is_hash_changed() {
      let content = self.generate_string()?;
      std::fs::File::create(out)?.write_all(content.as_bytes())?
    }

    Ok(())
  }
}
