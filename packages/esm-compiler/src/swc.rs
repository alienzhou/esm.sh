use crate::error::{DiagnosticBuffer, ErrorBuffer};
use crate::resolve_fold::resolve_fold;
use crate::resolver::{DependencyDescriptor, Resolver};
use crate::source_type::SourceType;

use std::{cell::RefCell, path::Path, rc::Rc};
use swc_common::{
	chain,
	comments::SingleThreadedComments,
	errors::{Handler, HandlerFlags},
	FileName, Globals, Mark, SourceMap,
};
use swc_ecma_transforms_proposal::decorators;
use swc_ecma_transforms_typescript::strip;
use swc_ecmascript::{
	ast::{EsVersion, Module, Program},
	codegen::{text_writer::JsWriter, Node},
	parser::{lexer::Lexer, EsConfig, StringInput, Syntax, TsConfig},
	transforms::{fixer, helpers, hygiene, pass::Optional, react, resolver_with_mark},
	visit::{Fold, FoldWith},
};

/// Options for transpiling a module.
#[derive(Debug, Clone)]
pub struct EmitOptions {
	pub jsx_import_source: Option<String>,
	pub jsx_factory: String,
	pub jsx_fragment_factory: String,
	pub is_dev: bool,
}

impl Default for EmitOptions {
	fn default() -> Self {
		EmitOptions {
			jsx_import_source: None,
			jsx_factory: "React.createElement".into(),
			jsx_fragment_factory: "React.Fragment".into(),
			is_dev: false,
		}
	}
}

#[derive(Clone)]
pub struct SWC {
	pub specifier: String,
	pub module: Module,
	pub source_type: SourceType,
	pub source_map: Rc<SourceMap>,
	pub comments: SingleThreadedComments,
}

impl SWC {
	/// parse source code.
	pub fn parse(
		specifier: &str,
		source: &str,
		source_type: Option<SourceType>,
	) -> Result<Self, anyhow::Error> {
		let source_map = SourceMap::default();
		let source_file = source_map.new_source_file(
			FileName::Real(Path::new(specifier).to_path_buf()),
			source.into(),
		);
		let sm = &source_map;
		let error_buffer = ErrorBuffer::new(specifier);
		let source_type = match source_type {
			Some(source_type) => match source_type {
				SourceType::Unknown => SourceType::from(Path::new(specifier)),
				_ => source_type,
			},
			None => SourceType::from(Path::new(specifier)),
		};
		let syntax = get_syntax(&source_type);
		let input = StringInput::from(&*source_file);
		let comments = SingleThreadedComments::default();
		let lexer = Lexer::new(syntax, EsVersion::Es2020, input, Some(&comments));
		let mut parser = swc_ecmascript::parser::Parser::new_from(lexer);
		let handler = Handler::with_emitter_and_flags(
			Box::new(error_buffer.clone()),
			HandlerFlags {
				can_emit_warnings: true,
				dont_buffer_diagnostics: true,
				..HandlerFlags::default()
			},
		);
		let module = parser
			.parse_module()
			.map_err(move |err| {
				let mut diagnostic = err.into_diagnostic(&handler);
				diagnostic.emit();
				DiagnosticBuffer::from_error_buffer(error_buffer, |span| sm.lookup_char_pos(span.lo))
			})
			.unwrap();

		Ok(SWC {
			specifier: specifier.into(),
			module,
			source_type,
			source_map: Rc::new(source_map),
			comments,
		})
	}

	/// transform a JS/TS/JSX/TSX file into a JS file, based on the supplied options.
	pub fn transform(
		self,
		resolver: Rc<RefCell<Resolver>>,
		options: &EmitOptions,
	) -> Result<(String, Option<String>), anyhow::Error> {
		swc_common::GLOBALS.set(&Globals::new(), || {
			let top_level_mark = Mark::fresh(Mark::root());
			let specifier_is_remote = resolver.borrow().specifier_is_remote;
			let is_jsx = match self.source_type {
				SourceType::JSX => true,
				SourceType::TSX => true,
				_ => false,
			};
			let passes = chain!(
				resolver_with_mark(top_level_mark),
				resolve_fold(resolver.clone()),
				decorators::decorators(decorators::Config {
					legacy: true,
					emit_metadata: false
				}),
				helpers::inject_helpers(),
				Optional::new(
					strip::strip_with_config(strip_config_from_emit_options(&options)),
					!is_jsx
				),
				Optional::new(
					strip::strip_with_jsx(
						self.source_map.clone(),
						strip_config_from_emit_options(&options),
						&self.comments,
						top_level_mark
					),
					is_jsx
				),
				Optional::new(
					react::refresh(
						options.is_dev,
						Some(react::RefreshOptions {
							refresh_reg: "$RefreshReg$".into(),
							refresh_sig: "$RefreshSig$".into(),
							emit_full_signatures: false,
						}),
						self.source_map.clone(),
						Some(&self.comments),
					),
					!specifier_is_remote
				),
				Optional::new(
					react::jsx(
						self.source_map.clone(),
						Some(&self.comments),
						react::Options {
							runtime: if options.jsx_import_source.is_some() {
								Some(react::Runtime::Automatic)
							} else {
								None
							},
							import_source: options.jsx_import_source.clone().unwrap_or_default(),
							pragma: options.jsx_factory.clone(),
							pragma_frag: options.jsx_fragment_factory.clone(),
							// this will use `Object.assign()` instead of the `_extends` helper when spreading props.
							use_builtins: true,
							development: options.is_dev,
							..Default::default()
						},
						top_level_mark
					),
					is_jsx
				),
				fixer(Some(&self.comments)),
				hygiene()
			);

			let (code, map) = self.apply_fold(passes, options.is_dev).unwrap();
			let mut resolver = resolver.borrow_mut();

			// remove unused deps by tree-shaking
			let mut deps: Vec<DependencyDescriptor> = Vec::new();
			for dep in resolver.deps.clone() {
				if resolver.star_exports.contains(&dep.specifier)
					|| code.contains(to_str_lit(dep.specifier.as_str()).as_str())
				{
					deps.push(dep);
				}
			}
			resolver.deps = deps;

			Ok((code, map))
		})
	}

	/// Apply transform with the fold.
	pub fn apply_fold<T: Fold>(
		&self,
		mut fold: T,
		source_map: bool,
	) -> Result<(String, Option<String>), anyhow::Error> {
		let program = Program::Module(self.module.clone());
		let program = helpers::HELPERS.set(&helpers::Helpers::new(false), || {
			program.fold_with(&mut fold)
		});
		let mut buf = Vec::new();
		let mut src_map_buf = Vec::new();
		let src_map = if source_map {
			Some(&mut src_map_buf)
		} else {
			None
		};
		{
			let writer = Box::new(JsWriter::new(
				self.source_map.clone(),
				"\n",
				&mut buf,
				src_map,
			));
			let mut emitter = swc_ecmascript::codegen::Emitter {
				cfg: swc_ecmascript::codegen::Config {
					minify: false,
				},
				comments: Some(&self.comments),
				cm: self.source_map.clone(),
				wr: writer,
			};
			program.emit_with(&mut emitter).unwrap();
		}

		// output
		let src = String::from_utf8(buf).unwrap();
		if source_map {
			let mut buf = Vec::new();
			self
				.source_map
				.build_source_map_from(&mut src_map_buf, None)
				.to_writer(&mut buf)
				.unwrap();
			Ok((src, Some(String::from_utf8(buf).unwrap())))
		} else {
			Ok((src, None))
		}
	}
}

fn get_es_config(jsx: bool) -> EsConfig {
	EsConfig {
		class_private_methods: true,
		class_private_props: true,
		class_props: true,
		dynamic_import: true,
		export_default_from: true,
		export_namespace_from: true,
		num_sep: true,
		nullish_coalescing: true,
		optional_chaining: true,
		top_level_await: true,
		import_meta: true,
		import_assertions: true,
		jsx,
		..EsConfig::default()
	}
}

fn get_ts_config(tsx: bool) -> TsConfig {
	TsConfig {
		decorators: true,
		dynamic_import: true,
		tsx,
		..TsConfig::default()
	}
}

fn get_syntax(source_type: &SourceType) -> Syntax {
	match source_type {
		SourceType::JS => Syntax::Es(get_es_config(false)),
		SourceType::JSX => Syntax::Es(get_es_config(true)),
		SourceType::TS => Syntax::Typescript(get_ts_config(false)),
		SourceType::TSX => Syntax::Typescript(get_ts_config(true)),
		_ => Syntax::Typescript(get_ts_config(true)),
	}
}

fn strip_config_from_emit_options(options: &EmitOptions) -> strip::Config {
	strip::Config {
		pragma: Some(options.jsx_factory.clone()),
		pragma_frag: Some(options.jsx_fragment_factory.clone()),
		import_not_used_as_values: strip::ImportsNotUsedAsValues::Remove,
		use_define_for_class_fields: true,
		// TODO(bartlomieju): this could be changed to `false` to provide `export {}`
		// in Typescript files without manual changes
		no_empty_export: true,
	}
}

fn to_str_lit(sub_text: &str) -> String {
	let mut s = "\"".to_owned();
	s.push_str(sub_text);
	s.push('"');
	s
}