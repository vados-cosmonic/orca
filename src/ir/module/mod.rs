//! Intermediate Representation of a wasm module.

use super::types::{DataType, InitExpr, Instruction, InstrumentationMode};
use crate::error::Error;
use crate::ir::function::FunctionModifier;
use crate::ir::id::{DataSegmentID, FunctionID, GlobalID, ImportsID, LocalID, MemoryID, TypeID};
use crate::ir::module::module_exports::{Export, ModuleExports};
use crate::ir::module::module_functions::{
    add_local, FuncKind, Function, Functions, ImportedFunction, LocalFunction,
};
use crate::ir::module::module_globals::{
    Global, GlobalKind, ImportedGlobal, LocalGlobal, ModuleGlobals,
};
use crate::ir::module::module_imports::{Import, ModuleImports};
use crate::ir::module::module_memories::{ImportedMemory, LocalMemory, MemKind, Memories, Memory};
use crate::ir::module::module_tables::ModuleTables;
use crate::ir::module::module_types::{ModuleTypes, Types};
use crate::ir::types::InstrumentationMode::{BlockAlt, BlockEntry, BlockExit, SemanticAfter};
use crate::ir::types::{
    BlockType, Body, CustomSections, DataSegment, DataSegmentKind, ElementItems, ElementKind,
    InstrumentationFlag,
};
use crate::ir::wrappers::{
    indirect_namemap_parser2encoder, namemap_parser2encoder, refers_to_func, refers_to_global,
    refers_to_memory, update_fn_instr, update_global_instr, update_memory_instr,
};
use crate::opcode::{Inject, Instrumenter};
use crate::{Location, Opcode};
use log::{error, warn};
use std::borrow::Cow;
use std::collections::HashMap;
use std::vec::IntoIter;
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::TagSection;
use wasmparser::Operator::Block;
use wasmparser::{
    CompositeInnerType, ExternalKind, GlobalType, MemoryType, Operator, Parser, Payload, TagType,
    TypeRef,
};

pub mod module_exports;
pub mod module_functions;
pub mod module_globals;
pub mod module_imports;
pub mod module_memories;
pub mod module_tables;
pub mod module_types;
#[cfg(test)]
mod test;

#[derive(Debug, Default)]
/// Intermediate Representation of a wasm module. See the [WASM Spec] for different sections.
///
/// [WASM Spec]: https://webassembly.github.io/spec/core/binary/modules.html
pub struct Module<'a> {
    /// name of module
    pub module_name: Option<String>,
    /// Types
    pub types: ModuleTypes,
    /// Imports
    pub imports: ModuleImports<'a>,
    /// Mapping from function index to type index.
    /// Note that `|functions| == num_functions + num_imported_functions`
    pub functions: Functions<'a>,
    /// Each table has a type and optional initialization expression.
    pub tables: ModuleTables<'a>,
    /// Memories
    pub memories: Memories,
    /// Globals
    pub globals: ModuleGlobals,
    /// Data Sections
    pub data: Vec<DataSegment>,
    data_count_section_exists: bool,
    /// Exports
    pub exports: ModuleExports,
    /// Index of the start function.
    pub start: Option<FunctionID>,
    /// Elements
    pub elements: Vec<(ElementKind<'a>, ElementItems<'a>)>,
    /// Tags
    pub tags: Vec<TagType>,
    /// Custom Sections
    pub custom_sections: CustomSections<'a>,
    /// Number of local functions (not counting imported functions)
    pub(crate) num_local_functions: u32,
    /// Number of local globals (not counting imported globals)
    pub(crate) num_local_globals: u32,
    /// Number of local tables (not counting imported tables)
    #[allow(dead_code)]
    pub(crate) num_local_tables: u32,
    /// Number of local memories (not counting imported memories)
    #[allow(dead_code)]
    pub(crate) num_local_memories: u32,

    // just a placeholder for round-trip
    pub(crate) local_names: wasm_encoder::IndirectNameMap,
    pub(crate) label_names: wasm_encoder::IndirectNameMap,
    pub(crate) type_names: wasm_encoder::NameMap,
    pub(crate) table_names: wasm_encoder::NameMap,
    pub(crate) memory_names: wasm_encoder::NameMap,
    pub(crate) global_names: wasm_encoder::NameMap,
    pub(crate) elem_names: wasm_encoder::NameMap,
    pub(crate) data_names: wasm_encoder::NameMap,
    pub(crate) field_names: wasm_encoder::IndirectNameMap,
    pub(crate) tag_names: wasm_encoder::NameMap,
}

impl<'a> Module<'a> {
    /// Parses a `Module` from a wasm binary.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use orca_wasm::Module;
    ///
    /// let file = "path_to_file";
    /// let buff = wat::parse_file(file).expect("couldn't convert the input wat to Wasm");
    /// let module = Module::parse(&buff, false).unwrap();
    /// ```
    pub fn parse(wasm: &'a [u8], enable_multi_memory: bool) -> Result<Self, Error> {
        let parser = Parser::new(0);
        Module::parse_internal(wasm, enable_multi_memory, parser)
    }

    pub(crate) fn parse_internal(
        wasm: &'a [u8],
        enable_multi_memory: bool,
        parser: Parser,
    ) -> Result<Self, Error> {
        let mut imports: ModuleImports = ModuleImports::default();
        let mut types: Vec<Types> = vec![];
        let mut data = vec![];
        let mut tables = vec![];
        let mut memories = vec![];
        let mut functions = vec![];
        let mut elements = vec![];
        let mut code_section_count = 0;
        let mut code_sections = vec![];
        let mut globals = vec![];
        let mut exports = vec![];
        let mut start = None;
        let mut data_section_count = None;
        let mut custom_sections = vec![];
        let mut tags: Vec<TagType> = vec![];

        let mut module_name: Option<String> = None;
        // for the other names, we directly encode it without passing them into the IR
        let mut local_names = wasm_encoder::IndirectNameMap::new();
        let mut label_names = wasm_encoder::IndirectNameMap::new();
        let mut type_names = wasm_encoder::NameMap::new();
        let mut table_names = wasm_encoder::NameMap::new();
        let mut memory_names = wasm_encoder::NameMap::new();
        let mut global_names = wasm_encoder::NameMap::new();
        let mut elem_names = wasm_encoder::NameMap::new();
        let mut data_names = wasm_encoder::NameMap::new();
        let mut field_names = wasm_encoder::IndirectNameMap::new();
        let mut tag_names = wasm_encoder::NameMap::new();
        let mut recgroup_map = HashMap::new();

        for payload in parser.parse_all(wasm) {
            let payload = payload?;
            match payload {
                Payload::ImportSection(import_section_reader) => {
                    let mut temp = vec![];
                    // count number of imported functions
                    for import in import_section_reader.into_iter() {
                        let imp = Import::from(import?);
                        temp.push(imp);
                    }
                    imports = ModuleImports::new(temp);
                }
                Payload::TypeSection(type_section_reader) => {
                    let mut ty_idx: u32 = 0;
                    for (id, ty) in type_section_reader.into_iter().enumerate() {
                        let rec_group = ty.clone()?.is_explicit_rec_group();
                        for subtype in ty?.types() {
                            match subtype.composite_type.inner.clone() {
                                CompositeInnerType::Func(fty) => {
                                    let fun_ty = fty;
                                    let params = fun_ty
                                        .params()
                                        .iter()
                                        .map(|x| DataType::from(*x))
                                        .collect::<Vec<_>>()
                                        .into_boxed_slice();
                                    let results = fun_ty
                                        .results()
                                        .iter()
                                        .map(|x| DataType::from(*x))
                                        .collect::<Vec<_>>()
                                        .into_boxed_slice();
                                    let final_ty = Types::FuncType {
                                        params,
                                        results,
                                        super_type: subtype.supertype_idx,
                                        is_final: subtype.is_final,
                                        shared: subtype.composite_type.shared,
                                    };
                                    types.push(final_ty.clone());

                                    if rec_group {
                                        recgroup_map.insert(ty_idx, id as u32);
                                    }
                                }
                                CompositeInnerType::Array(aty) => {
                                    let array_ty = Types::ArrayType {
                                        mutable: aty.0.mutable,
                                        fields: DataType::from(aty.0.element_type),
                                        super_type: subtype.supertype_idx,
                                        is_final: subtype.is_final,
                                        shared: subtype.composite_type.shared,
                                    };
                                    types.push(array_ty.clone());

                                    if rec_group {
                                        recgroup_map.insert(ty_idx, id as u32);
                                    }
                                }
                                CompositeInnerType::Struct(sty) => {
                                    let struct_ty = Types::StructType {
                                        mutable: sty
                                            .fields
                                            .iter()
                                            .map(|field| field.mutable)
                                            .collect::<Vec<_>>(),
                                        fields: sty
                                            .fields
                                            .iter()
                                            .map(|field| DataType::from(field.element_type))
                                            .collect::<Vec<_>>(),
                                        super_type: subtype.supertype_idx,
                                        is_final: subtype.is_final,
                                        shared: subtype.composite_type.shared,
                                    };
                                    types.push(struct_ty.clone());
                                    if rec_group {
                                        recgroup_map.insert(ty_idx, id as u32);
                                    }
                                }
                                CompositeInnerType::Cont(cty) => {
                                    let cont_ty = Types::ContType {
                                        packed_index: cty.0,
                                        super_type: subtype.supertype_idx,
                                        is_final: subtype.is_final,
                                        shared: subtype.composite_type.shared,
                                    };
                                    types.push(cont_ty.clone());
                                    if rec_group {
                                        recgroup_map.insert(ty_idx, id as u32);
                                    }
                                }
                            }
                            ty_idx += 1;
                        }
                    }
                }
                Payload::DataSection(data_section_reader) => {
                    data = data_section_reader
                        .into_iter()
                        .map(|sec| {
                            sec.map_err(Error::from)
                                .and_then(DataSegment::from_wasmparser)
                        })
                        .collect::<Result<_, _>>()?;
                }
                Payload::TableSection(table_section_reader) => {
                    tables = table_section_reader
                        .into_iter()
                        .map(|t| {
                            t.map_err(Error::from).map(|t| match t.init {
                                wasmparser::TableInit::RefNull => (t.ty, None),
                                wasmparser::TableInit::Expr(e) => (t.ty, Some(e)),
                            })
                        })
                        .collect::<Result<_, _>>()?;
                }
                Payload::MemorySection(memory_section_reader) => {
                    memories = memory_section_reader
                        .into_iter()
                        .collect::<Result<_, _>>()?;
                }
                Payload::FunctionSection(function_section_reader) => {
                    let temp: Vec<u32> = function_section_reader
                        .into_iter()
                        .collect::<Result<_, _>>()?;
                    functions.extend(temp.iter().map(|id| TypeID(*id)));
                }
                Payload::GlobalSection(global_section_reader) => {
                    globals = global_section_reader
                        .into_iter()
                        .map(|g| Global::from_wasmparser(g?))
                        .collect::<Result<_, _>>()?;
                }
                Payload::ExportSection(export_section_reader) => {
                    for exp in export_section_reader.into_iter() {
                        exports.push(Export::from(exp?));
                    }
                }
                Payload::StartSection { func, range: _ } => {
                    if start.is_some() {
                        return Err(Error::MultipleStartSections);
                    }
                    start = Some(FunctionID(func));
                }
                Payload::ElementSection(element_section_reader) => {
                    for element in element_section_reader.into_iter() {
                        let element = element?;
                        let items = ElementItems::from_wasmparser(element.items.clone())?;
                        elements.push((ElementKind::from_wasmparser(element.kind)?, items));
                    }
                }
                Payload::DataCountSection { count, range: _ } => {
                    data_section_count = Some(count);
                }
                Payload::CodeSectionStart {
                    count,
                    range: _,
                    size: _,
                } => {
                    code_section_count = count as usize;
                }
                Payload::CodeSectionEntry(body) => {
                    let locals_reader = body.get_locals_reader()?;
                    let locals = locals_reader.into_iter().collect::<Result<Vec<_>, _>>()?;
                    let mut num_locals = 0;
                    let locals: Vec<(u32, DataType)> = locals
                        .iter()
                        .map(|(count, val_type)| {
                            num_locals += count;
                            (*count, DataType::from(*val_type))
                        })
                        .collect();

                    let instructions = body
                        .get_operators_reader()?
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()?;
                    if let Some(last) = instructions.last() {
                        if let Operator::End = last {
                        } else {
                            return Err(Error::MissingFunctionEnd {
                                func_range: body.range(),
                            });
                        }
                    }
                    if !enable_multi_memory
                        && instructions.iter().any(|i| match i {
                            Operator::MemoryGrow { mem, .. } | Operator::MemorySize { mem, .. } => {
                                *mem != 0x00
                            }
                            _ => false,
                        })
                    {
                        return Err(Error::InvalidMemoryReservedByte {
                            func_range: body.range(),
                        });
                    }
                    let instructions_bool: Vec<_> =
                        instructions.into_iter().map(Instruction::new).collect();
                    code_sections.push(Body {
                        locals,
                        num_locals,
                        instructions: instructions_bool.clone(),
                        num_instructions: instructions_bool.len(),
                        name: None,
                    });
                }
                Payload::TagSection(tag_section_reader) => {
                    for tag in tag_section_reader.into_iter() {
                        match tag {
                            Ok(t) => tags.push(t),
                            Err(e) => panic!("Error encored in tag section!: {}", e),
                        }
                    }
                }
                Payload::CustomSection(custom_section_reader) => {
                    match custom_section_reader.as_known() {
                        wasmparser::KnownCustom::Name(name_section_reader) => {
                            for subsection in name_section_reader {
                                #[allow(clippy::single_match)]
                                match subsection? {
                                    wasmparser::Name::Function(names) => {
                                        for name in names {
                                            let naming = name?;
                                            let abs_idx = naming.index;
                                            if abs_idx < imports.num_funcs {
                                                let mut import_func_count = 0;
                                                // TODO: this is very expensive, can we optimize this?
                                                for import in imports.iter_mut() {
                                                    if import.is_function() {
                                                        if import_func_count == abs_idx {
                                                            import.custom_name =
                                                                Some(naming.name.to_string());
                                                            break;
                                                        }
                                                        import_func_count += 1;
                                                    }
                                                }
                                            } else {
                                                let rel_idx = abs_idx - imports.num_funcs;
                                                // assert!(0 < rel_idx && rel_idx < code_sections.len() as u32);
                                                code_sections[rel_idx as usize].name =
                                                    Some(naming.name.to_string());
                                            }
                                        }
                                    }
                                    wasmparser::Name::Module { name, .. } => {
                                        module_name = Some(name.to_string());
                                    }
                                    wasmparser::Name::Local(names) => {
                                        local_names = indirect_namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Label(names) => {
                                        label_names = indirect_namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Type(names) => {
                                        type_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Table(names) => {
                                        table_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Memory(names) => {
                                        memory_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Global(names) => {
                                        global_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Element(names) => {
                                        elem_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Data(names) => {
                                        data_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Field(names) => {
                                        field_names = indirect_namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Tag(names) => {
                                        tag_names = namemap_parser2encoder(names);
                                    }
                                    wasmparser::Name::Unknown { .. } => {}
                                }
                            }
                        }
                        wasmparser::KnownCustom::Producers(producer_section_reader) => {
                            let field = producer_section_reader
                                .into_iter()
                                .next()
                                .unwrap()
                                .expect("producers field");
                            let _value = field
                                .values
                                .into_iter()
                                .collect::<Result<Vec<_>, _>>()
                                .expect("values");
                            custom_sections
                                .push((custom_section_reader.name(), custom_section_reader.data()));
                        }
                        _ => {
                            custom_sections
                                .push((custom_section_reader.name(), custom_section_reader.data()));
                        }
                    }
                }
                Payload::Version {
                    num,
                    encoding: _,
                    range: _,
                } => {
                    if num != 1 {
                        return Err(Error::UnknownVersion(num as u32));
                    }
                }
                Payload::UnknownSection {
                    id,
                    contents: _,
                    range: _,
                } => return Err(Error::UnknownSection { section_id: id }),
                Payload::ModuleSection {
                    parser: _,
                    unchecked_range: _,
                }
                | Payload::InstanceSection(_)
                | Payload::CoreTypeSection(_)
                | Payload::ComponentSection {
                    parser: _,
                    unchecked_range: _,
                }
                | Payload::ComponentInstanceSection(_)
                | Payload::ComponentAliasSection(_)
                | Payload::ComponentTypeSection(_)
                | Payload::ComponentCanonicalSection(_)
                | Payload::ComponentStartSection { start: _, range: _ }
                | Payload::ComponentImportSection(_)
                | Payload::ComponentExportSection(_)
                | Payload::End(_) => {}
                _ => todo!(),
            }
        }
        if code_section_count != code_sections.len() || code_section_count != functions.len() {
            return Err(Error::IncorrectCodeCounts {
                function_section_count: functions.len(),
                code_section_declared_count: code_section_count,
                code_section_actual_count: code_sections.len(),
            });
        }
        if let Some(data_count) = data_section_count {
            if data_count as usize != data.len() {
                return Err(Error::IncorrectDataCount {
                    declared_count: data_count as usize,
                    actual_count: data.len(),
                });
            }
        }

        // Add all the functions. First add all the imported functions as they have the first IDs
        let mut final_funcs = vec![];
        let mut imp_fn_id = 0;
        for (index, imp) in imports.iter().enumerate() {
            if let TypeRef::Func(u) = imp.ty {
                final_funcs.push(Function::new(
                    FuncKind::Import(ImportedFunction::new(
                        ImportsID(index as u32),
                        TypeID(u),
                        FunctionID(imp_fn_id),
                    )),
                    Some(imp.name.parse().unwrap()),
                ));
                imp_fn_id += 1;
            }
        }
        // Local Functions
        for (index, code_sec) in code_sections.iter().enumerate() {
            final_funcs.push(Function::new(
                FuncKind::Local(LocalFunction::new(
                    functions[index],
                    FunctionID(imports.num_funcs + index as u32),
                    (*code_sec).clone(),
                    types[*functions[index] as usize].params().len(),
                )),
                (*code_sec).clone().name,
            ));
        }

        // Process the imported memories
        let mut final_mems = vec![];
        let mut imp_mem_id = 0;
        for (index, imp) in imports.iter().enumerate() {
            if let TypeRef::Memory(ty) = imp.ty {
                final_mems.push(Memory::new(
                    ty,
                    MemKind::Import(ImportedMemory {
                        import_id: ImportsID(index as u32),
                        import_mem_id: MemoryID(imp_mem_id),
                    }),
                ));
                imp_mem_id += 1;
            }
        }
        // Process the Local memories
        for (index, ty) in memories.iter().enumerate() {
            final_mems.push(Memory::new(
                ty.to_owned(),
                MemKind::Local(LocalMemory {
                    mem_id: MemoryID(imports.num_memories + index as u32),
                }),
            ));
        }

        let num_globals = globals.len() as u32;
        let num_memories = memories.len() as u32;
        let num_tables = tables.len() as u32;
        let module_globals = ModuleGlobals::new(&imports, globals);
        Ok(Module {
            types: ModuleTypes::new(types, recgroup_map),
            imports,
            functions: Functions::new(final_funcs),
            tables: ModuleTables::new(tables),
            memories: Memories::new(final_mems),
            globals: module_globals,
            exports: ModuleExports::new(exports),
            start,
            elements,
            data_count_section_exists: data_section_count.is_some(),
            // code_sections: code_sections.clone(),
            data,
            tags,
            custom_sections: CustomSections::new(custom_sections),
            num_local_functions: code_sections.len() as u32,
            num_local_globals: num_globals,
            num_local_tables: num_tables,
            num_local_memories: num_memories,
            module_name,
            local_names,
            type_names,
            table_names,
            elem_names,
            memory_names,
            global_names,
            data_names,
            field_names,
            tag_names,
            label_names,
        })
    }

    /// Creates Vec of (Function, Number of Instructions)
    pub fn get_func_metadata(&self) -> Vec<(FunctionID, usize)> {
        let mut metadata = vec![];
        for func in self.functions.iter() {
            match &func.kind {
                FuncKind::Import(_) => {}
                FuncKind::Local(LocalFunction { func_id, body, .. }) => {
                    metadata.push((*func_id, body.num_instructions));
                }
            }
        }
        metadata
    }

    /// Emit the module into a wasm binary file.
    pub fn emit_wasm(&mut self, file_name: &str) -> Result<(), std::io::Error> {
        let module = self.encode_internal();
        let wasm = module.finish();
        std::fs::write(file_name, wasm)?;
        Ok(())
    }

    /// Encode the module into a wasm binary.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use orca_wasm::Module;
    ///
    /// let file = "path_to_file";
    /// let buff = wat::parse_file(file).expect("couldn't convert the input wat to Wasm");
    /// let mut module = Module::parse(&buff, false).unwrap();
    /// let result = module.encode();
    /// ```
    pub fn encode(&mut self) -> Vec<u8> {
        self.encode_internal().finish()
    }

    /// Visits the Orca Module and resolves the special instrumentation by
    /// translating them into the straightforward before/after/alt modes.
    fn resolve_special_instrumentation(&mut self) {
        if !self.num_local_functions > 0 {
            for rel_func_idx in (self.imports.num_funcs - self.imports.num_funcs_added) as usize
                ..self.functions.len()
            {
                let func_idx = FunctionID(rel_func_idx as u32);
                if let FuncKind::Import(..) = &self.functions.get_kind(func_idx) {
                    // skip imports
                    continue;
                }

                let mut instr_func_on_entry = None;
                let mut instr_func_on_exit = None;
                if let FuncKind::Local(LocalFunction { instr_flag, .. }) =
                    self.functions.get_kind_mut(func_idx)
                {
                    if !instr_flag.has_special_instr {
                        // skip functions without special instrumentation!
                        continue;
                    }

                    // save off the function entry/exit special mode bodies
                    if !instr_flag.entry.is_empty() {
                        instr_func_on_entry = Some(instr_flag.entry.to_owned());
                        instr_flag.entry = vec![];
                    }
                    if !instr_flag.exit.is_empty() {
                        instr_func_on_exit = Some(instr_flag.exit.to_owned());
                        instr_flag.exit = vec![];
                    }
                }

                // initialize with 0 to store the func block!
                let mut block_stack: Vec<BlockID> = vec![0];
                let mut delete_block: Option<BlockID> = None;
                let mut retain_end = true;
                let mut resolve_on_else_or_end: HashMap<InstrumentationMode, InstrToInject> =
                    HashMap::new();
                let mut resolve_on_end: HashMap<
                    BlockID,
                    HashMap<InstrumentationMode, InstrToInject>,
                > = HashMap::new();
                if let Some(on_exit) = &mut instr_func_on_exit {
                    if !on_exit.is_empty() {
                        let on_entry = if let Some(on_entry) = &mut instr_func_on_entry {
                            on_entry
                        } else {
                            let on_entry = vec![];
                            instr_func_on_entry = Some(on_entry);
                            if let Some(ref mut on_entry) = instr_func_on_entry {
                                on_entry
                            } else {
                                panic!()
                            }
                        };

                        let func_ty = self.functions.get_type_id(func_idx);
                        let func_results = self.types.get(func_ty).unwrap().results();
                        let block_ty = self.types.add_func_type(&[], &func_results);
                        resolve_function_exit_with_block_wrapper(on_entry, block_ty);
                    }
                }
                let mut builder = self.functions.get_fn_modifier(func_idx).unwrap();

                // Must make copy to be able to iterate over body while calling builder.* methods that mutate the instrumentation flag!
                let readable_copy_of_body = builder.body.instructions.clone();
                for (
                    idx,
                    Instruction {
                        op,
                        instr_flag: instrumentation,
                    },
                ) in readable_copy_of_body.iter().enumerate()
                {
                    // resolve function-level instrumentation
                    if let Some(on_entry) = &mut instr_func_on_entry {
                        if !on_entry.is_empty() {
                            resolve_function_entry(&mut builder, on_entry, idx);
                        }
                    }
                    if let Some(on_exit) = &mut instr_func_on_exit {
                        if !on_exit.is_empty() {
                            resolve_function_exit(on_exit, &mut builder, op, idx);
                        }
                    }

                    // resolve instruction-level instrumentation
                    match op {
                        Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                            // The block ID will just be the curr len of the stack!
                            block_stack.push(block_stack.len() as u32);

                            // Handle block alt
                            if let Some(block_alt) = &instrumentation.block_alt {
                                // only plan to handle if we're not already removing the block this instr is in
                                if delete_block.is_none()
                                    && plan_resolution_block_alt(
                                        block_alt,
                                        &mut builder,
                                        &mut retain_end,
                                        op,
                                        idx,
                                    )
                                {
                                    builder.clear_instr_at(
                                        Location::Module {
                                            func_idx: FunctionID(0), // not used
                                            instr_idx: idx,
                                        },
                                        BlockAlt,
                                    );
                                    // we've got a match, which injected the alt body. continue to the next instruction
                                    delete_block = Some(*block_stack.last().unwrap());
                                    continue;
                                }
                            }

                            if delete_block.is_some() {
                                // delete this block and skip all instrumentation handling (like below)
                                builder.empty_alternate_at(Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                });
                                continue;
                            }
                        }
                        Operator::Else => {
                            // necessary for if statements with block_exit instrumentation
                            for (mode, instr_to_inject) in resolve_on_else_or_end.iter() {
                                // resolve bodies at the else
                                resolve_bodies(&mut builder, mode, instr_to_inject, idx);
                            }
                            resolve_on_else_or_end.clear();

                            // Handle block alt
                            if let Some(block_alt) = &instrumentation.block_alt {
                                // only plan to handle if we're not already removing the block this instr is in
                                if delete_block.is_none()
                                    && plan_resolution_block_alt(
                                        block_alt,
                                        &mut builder,
                                        &mut retain_end,
                                        op,
                                        idx,
                                    )
                                {
                                    builder.clear_instr_at(
                                        Location::Module {
                                            func_idx: FunctionID(0), // not used
                                            instr_idx: idx,
                                        },
                                        BlockAlt,
                                    );
                                    // we've got a match, which injected the alt body. continue to the next instruction
                                    delete_block = Some(*block_stack.last().unwrap());
                                    continue;
                                }
                            }

                            if delete_block.is_some() {
                                // delete this block and skip all instrumentation handling (like below)
                                builder.empty_alternate_at(Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                });
                                continue;
                            }
                        }
                        Operator::End => {
                            // Pop the stack and check to see if we have instrumentation to inject!
                            if let Some(block_id) = block_stack.pop() {
                                if let Some(delete_block_id) = delete_block.as_mut() {
                                    // Delete the block, but don't remove the end if we say not to
                                    // should still process instrumentation on the end though...
                                    // (consider if/else where the else has an alt block)
                                    if (*delete_block_id).eq(&block_id) {
                                        // completing the alt block logic, clear state
                                        delete_block = None;
                                        if !retain_end {
                                            // delete this end and skip all instrumentation handling (like below)
                                            builder.empty_alternate_at(Location::Module {
                                                func_idx: FunctionID(0), // not used
                                                instr_idx: idx,
                                            });
                                            retain_end = true;
                                            continue;
                                        }
                                        // fall through to the instrumentation handling
                                        retain_end = true;
                                    } else {
                                        // delete this instruction and skip all instrumentation handling (like below)
                                        builder.empty_alternate_at(Location::Module {
                                            func_idx: FunctionID(0), // not used
                                            instr_idx: idx,
                                        });
                                        continue;
                                    }
                                }

                                // we've reached an end, make sure resolve_on_else is cleared!
                                // resolve bodies for else OR end
                                for (mode, instr_to_inject) in resolve_on_else_or_end.iter() {
                                    resolve_bodies(&mut builder, mode, instr_to_inject, idx);
                                }
                                resolve_on_else_or_end.clear();

                                // remove top of stack! (end of vec)
                                // remove it, so we don't try to re-inject!
                                if let Some(to_resolve) = resolve_on_end.remove(&block_id) {
                                    for (mode, instr_to_inject) in to_resolve.iter() {
                                        // resolve bodies at the end
                                        resolve_bodies(&mut builder, mode, instr_to_inject, idx);
                                    }
                                }
                            }
                        }
                        _ => {
                            // non block-structured opcodes
                            if delete_block.is_some() {
                                // delete this instruction and skip all instrumentation handling (like below)
                                builder.empty_alternate_at(Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                });
                                continue;
                            }
                        }
                    }

                    // plan instruction-level instrumentation resolution
                    // this must go after the above logic to ensure the block_id is on the top of the stack!
                    if instrumentation.has_instr() {
                        // this instruction has instrumentation, check if there is any to resolve!
                        let InstrumentationFlag {
                            semantic_after,
                            block_entry,
                            block_exit,
                            block_alt: _, // handled before here!
                            before: _,
                            after: _,
                            alternate: _,
                            current_mode: _,
                            // exhaustive to help identify where to add code to handle other special modes.
                        } = instrumentation;

                        // Handle block entry
                        if !block_entry.is_empty() {
                            resolve_block_entry(block_entry, &mut builder, op, idx);
                            builder.clear_instr_at(
                                Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                },
                                BlockEntry,
                            );
                        }

                        // Handle block exit
                        if !block_exit.is_empty() {
                            plan_resolution_block_exit(
                                block_exit,
                                &block_stack,
                                &mut resolve_on_else_or_end,
                                &mut resolve_on_end,
                                op,
                            );
                            builder.clear_instr_at(
                                Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                },
                                BlockExit,
                            );
                        }

                        // Handle semantic_after!
                        if !semantic_after.is_empty() {
                            plan_resolution_semantic_after(
                                semantic_after,
                                &mut builder,
                                &block_stack,
                                &mut resolve_on_end,
                                op,
                                idx,
                            );
                            builder.clear_instr_at(
                                Location::Module {
                                    func_idx: FunctionID(0), // not used
                                    instr_idx: idx,
                                },
                                SemanticAfter,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Reorganises items (both local and imports) in the correct ordering after any potential modifications
    pub(crate) fn reorganise_generic<T: LocalOrImport, U: ReIndexable<T>>(
        orig_num_imported: u32,
        items: &mut U,
        items_read_only: IntoIter<T>,
    ) {
        // Location where we may have to move an import (converted from local) to
        let mut num_imported = orig_num_imported;
        let mut num_deleted = 0;

        // Iterate over cloned list
        for (idx, val) in items_read_only.enumerate() {
            // If the index is less than < imported
            if idx < orig_num_imported as usize {
                // If it is a local, that means it was an import before
                if val.is_local() {
                    let f = items.remove((idx - num_deleted) as u32);
                    items.push(f);
                    // decrement as this is the place where we might have to move an import to
                    num_imported -= 1;
                    // We update it here for the following case. A , B. A is moved to a position later than B, indices will reduce by 1 and we need the offset
                    num_deleted += 1;
                } else if val.is_deleted() {
                    // If val was import but was deleted
                    items.remove((idx - num_deleted) as u32);
                    num_imported -= 1;
                    num_deleted += 1;
                }
            } else {
                // If it's an import, was a local before
                if val.is_import() {
                    let i = items.remove((idx - num_deleted) as u32);
                    items.insert(num_imported, i);
                    // increment as this is the place where we might have to move an import to
                    num_imported += 1;
                    // We do not update it here for the following case. A , B. A is moved to a position earlier than B, indices will not change and hence no need to update
                    // num_deleted += 1;
                }
                // If val was local but was deleted
                else if val.is_deleted() {
                    items.remove((idx - num_deleted) as u32);
                    num_deleted += 1;
                }
            }
        }
    }

    /// Get the mapping of old ID -> new ID in module
    pub(crate) fn get_mapping_generic<T: GetID>(
        slice: std::slice::Iter<'_, T>,
    ) -> HashMap<u32, u32> {
        let mut mapping = HashMap::new();
        for (new_id, item) in slice.enumerate() {
            let old_id = item.get_id();
            mapping.insert(old_id, new_id as u32);
        }
        mapping
    }

    pub(crate) fn recalculate_ids<T: LocalOrImport + GetID, U: Iter<T> + ReIndexable<T>>(
        orig_num_imported: u32,
        items: &mut U,
    ) -> HashMap<u32, u32> {
        let items_read_only = items.get_into_iter();
        Self::reorganise_generic(orig_num_imported, items, items_read_only);
        let id_mapping = Self::get_mapping_generic(items.iter());
        assert_eq!(items.len(), id_mapping.len());
        id_mapping
    }

    fn encode_type(&self, ty: &Types) -> wasm_encoder::SubType {
        match ty {
            Types::FuncType {
                params,
                results,
                super_type,
                is_final,
                shared,
            } => {
                let params = params
                    .iter()
                    .map(wasm_encoder::ValType::from)
                    .collect::<Vec<_>>();
                let results = results
                    .iter()
                    .map(wasm_encoder::ValType::from)
                    .collect::<Vec<_>>();
                let fty = wasm_encoder::FuncType::new(params, results);
                wasm_encoder::SubType {
                    is_final: *is_final,
                    supertype_idx: match super_type {
                        None => None,
                        Some(idx) => idx.as_module_index(),
                    },
                    composite_type: wasm_encoder::CompositeType {
                        inner: wasm_encoder::CompositeInnerType::Func(fty),
                        shared: *shared,
                    },
                }
            }
            Types::ArrayType {
                fields,
                mutable,
                super_type,
                is_final,
                shared,
            } => wasm_encoder::SubType {
                is_final: *is_final,
                supertype_idx: match super_type {
                    None => None,
                    Some(idx) => idx.as_module_index(),
                },
                composite_type: wasm_encoder::CompositeType {
                    inner: wasm_encoder::CompositeInnerType::Array(wasm_encoder::ArrayType(
                        wasm_encoder::FieldType {
                            element_type: wasm_encoder::StorageType::from(*fields),
                            mutable: *mutable,
                        },
                    )),
                    shared: *shared,
                },
            },
            Types::StructType {
                fields,
                mutable,
                super_type,
                is_final,
                shared,
            } => {
                let mut encoded_fields: Vec<wasm_encoder::FieldType> = vec![];
                for (idx, sty) in fields.iter().enumerate() {
                    encoded_fields.push(wasm_encoder::FieldType {
                        element_type: wasm_encoder::StorageType::from(*sty),
                        mutable: mutable[idx],
                    });
                }
                wasm_encoder::SubType {
                    is_final: *is_final,
                    supertype_idx: match super_type {
                        None => None,
                        Some(idx) => idx.as_module_index(),
                    },
                    composite_type: wasm_encoder::CompositeType {
                        inner: wasm_encoder::CompositeInnerType::Struct(wasm_encoder::StructType {
                            fields: Box::from(encoded_fields),
                        }),
                        shared: *shared,
                    },
                }
            }
            Types::ContType { .. } => {
                todo!()
            }
        }
    }

    /// Encodes an Orca Module to a wasm_encoder Module.
    /// This requires a mutable reference to self due to the special instrumentation resolution step.
    pub(crate) fn encode_internal(&mut self) -> wasm_encoder::Module {
        // First resolve any instrumentation that needs to be translated to before/after/alt
        self.resolve_special_instrumentation();

        let func_mapping = if self.functions.recalculate_ids {
            Self::recalculate_ids(
                self.imports.num_funcs - self.imports.num_funcs_added,
                &mut self.functions,
            )
        } else {
            Self::get_mapping_generic(Iter::<Function<'a>>::iter(&self.functions))
        };
        let global_mapping = if self.globals.recalculate_ids {
            Self::recalculate_ids(
                self.imports.num_globals - self.imports.num_globals_added,
                &mut self.globals,
            )
        } else {
            Self::get_mapping_generic(self.globals.iter())
        };
        let memory_mapping = if self.memories.recalculate_ids {
            Self::recalculate_ids(
                self.imports.num_memories - self.imports.num_memories_added,
                &mut self.memories,
            )
        } else {
            Self::get_mapping_generic(self.memories.iter())
        };

        let mut module = wasm_encoder::Module::new();
        let mut reencode = RoundtripReencoder;

        let new_start = if let Some(start_fn) = self.start {
            // fix the start function mapping
            match func_mapping.get(&*start_fn) {
                Some(new_index) => Some(FunctionID(*new_index)),
                None => {
                    warn!("Deleted the start function!");
                    None
                }
            }
        } else {
            None
        };
        self.start = new_start;

        if !self.types.is_empty() {
            let mut types = wasm_encoder::TypeSection::new();
            let mut last_rg = None;
            let mut rg_types = vec![];
            for (idx, ty) in self.types.iter().enumerate() {
                let curr_rg = self.types.recgroup_map.get(&(idx as u32));
                // If current one is not the same as last one and it is not the first rg, encode it
                // If it is a new one
                if curr_rg != last_rg {
                    // If the previous one was an explicit rec group
                    if last_rg.is_some() {
                        // Encode the last one as a recgroup
                        types.ty().rec(rg_types.clone());
                        // Reset the vector
                        rg_types.clear();
                    }
                    // If it was not, then it was already encoded
                }
                match curr_rg {
                    // If it is part of an explicit rec group
                    Some(_) => {
                        rg_types.push(self.encode_type(ty));
                        // first_rg = false;
                    }
                    None => types.ty().subtype(&self.encode_type(ty)),
                }
                last_rg = curr_rg;
            }
            // If the last rg was a none, it was encoded in the binary, if it was an explicit rec group, was not encoded
            if last_rg.is_some() {
                types.ty().rec(rg_types.clone());
            }
            module.section(&types);
        }

        // initialize function name section
        let mut function_names = wasm_encoder::NameMap::new();
        if !self.imports.is_empty() {
            let mut imports = wasm_encoder::ImportSection::new();
            let mut import_func_idx = 0;
            for import in self.imports.iter() {
                if !import.deleted {
                    if import.is_function() {
                        if let Some(import_name) = &import.custom_name {
                            function_names.append(import_func_idx as u32, import_name);
                        }
                        import_func_idx += 1;
                    }
                    imports.import(
                        import.module,
                        import.name,
                        reencode.entity_type(import.ty).unwrap(),
                    );
                }
            }
            module.section(&imports);
        }

        if !self.functions.is_empty() {
            let mut functions = wasm_encoder::FunctionSection::new();
            for func in self.functions.iter() {
                if !func.deleted {
                    if let FuncKind::Local(l) = func.kind() {
                        functions.function(*l.ty_id);
                    }
                }
            }
            module.section(&functions);
        }

        if !self.tables.is_empty() {
            let mut tables = wasm_encoder::TableSection::new();
            for (table_ty, init) in self.tables.iter() {
                let table_ty = wasm_encoder::TableType {
                    element_type: wasm_encoder::RefType {
                        nullable: table_ty.element_type.is_nullable(),
                        heap_type: reencode
                            .heap_type(table_ty.element_type.heap_type())
                            .unwrap(),
                    },
                    table64: table_ty.table64,
                    minimum: table_ty.initial, // TODO - Check if this maps
                    maximum: table_ty.maximum,
                    shared: table_ty.shared,
                };
                match init {
                    None => tables.table(table_ty),
                    Some(const_expr) => tables.table_with_init(
                        table_ty,
                        &reencode
                            .const_expr((*const_expr).clone())
                            .expect("Error in Converting Const Expr"),
                    ),
                };
            }
            module.section(&tables);
        }

        if !self.memories.is_empty() {
            let mut memories = wasm_encoder::MemorySection::new();
            for memory in self.memories.iter() {
                if memory.is_local() {
                    memories.memory(wasm_encoder::MemoryType::from(memory.ty));
                }
            }
            module.section(&memories);
        }

        if !self.globals.is_empty() {
            let mut globals = wasm_encoder::GlobalSection::new();
            for global in self.globals.iter() {
                if !global.deleted {
                    if let GlobalKind::Local(LocalGlobal { ty, init_expr, .. }) = &global.kind {
                        globals.global(
                            wasm_encoder::GlobalType {
                                val_type: reencode.val_type(ty.content_type).unwrap(),
                                mutable: ty.mutable,
                                shared: ty.shared,
                            },
                            &init_expr.to_wasmencoder_type(),
                        );
                    }
                }
                // skip imported globals
            }
            module.section(&globals);
        }

        if !self.exports.is_empty() {
            let mut exports = wasm_encoder::ExportSection::new();
            for export in self.exports.iter() {
                if !export.deleted {
                    match export.kind {
                        ExternalKind::Func => {
                            // Update the function indices
                            exports.export(
                                &export.name,
                                wasm_encoder::ExportKind::from(export.kind),
                                *func_mapping.get(&(export.index)).unwrap(),
                            );
                        }
                        ExternalKind::Memory => {
                            // Update the memory indices
                            exports.export(
                                &export.name,
                                wasm_encoder::ExportKind::from(export.kind),
                                *memory_mapping.get(&(export.index)).unwrap(),
                            );
                        }
                        _ => {
                            exports.export(
                                &export.name,
                                wasm_encoder::ExportKind::from(export.kind),
                                export.index,
                            );
                        }
                    }
                }
            }
            module.section(&exports);
        }

        if let Some(function_index) = self.start {
            module.section(&wasm_encoder::StartSection {
                function_index: *function_index,
            });
        }

        if !self.elements.is_empty() {
            let mut elements = wasm_encoder::ElementSection::new();
            let mut temp_const_exprs = vec![];
            let mut element_items = vec![];
            for (kind, items) in self.elements.iter() {
                temp_const_exprs.clear();
                element_items.clear();
                let element_items = match &items {
                    // TODO: Update the elements section based on additions/deletion
                    ElementItems::Functions(funcs) => {
                        element_items = funcs
                            .iter()
                            .map(|f| *func_mapping.get(f).unwrap())
                            .collect();
                        wasm_encoder::Elements::Functions(Cow::from(element_items.as_slice()))
                    }
                    ElementItems::ConstExprs { ty, exprs } => {
                        temp_const_exprs.reserve(exprs.len());
                        for e in exprs.iter() {
                            temp_const_exprs.push(
                                reencode
                                    .const_expr((*e).clone())
                                    .expect("Unable to convert element constant expr"),
                            );
                        }
                        wasm_encoder::Elements::Expressions(
                            wasm_encoder::RefType {
                                nullable: ty.is_nullable(),
                                heap_type: reencode.heap_type(ty.heap_type()).unwrap(),
                            },
                            Cow::from(&temp_const_exprs),
                        )
                    }
                };

                match kind {
                    ElementKind::Passive => {
                        elements.passive(element_items);
                    }
                    ElementKind::Active {
                        table_index,
                        offset_expr,
                    } => {
                        elements.active(
                            *table_index,
                            &reencode
                                .const_expr((*offset_expr).clone())
                                .expect("Unable to convert offset expr"),
                            element_items,
                        );
                    }
                    ElementKind::Declared => {
                        elements.declared(element_items);
                    }
                }
            }
            module.section(&elements);
        }

        if self.data_count_section_exists {
            let data_count = wasm_encoder::DataCountSection {
                count: self.data.len() as u32,
            };
            module.section(&data_count);
        }

        if !self.tags.is_empty() {
            let mut tags = TagSection::new();
            for tag in self.tags.iter() {
                tags.tag(wasm_encoder::TagType {
                    kind: wasm_encoder::TagKind::from(tag.kind),
                    func_type_idx: tag.func_type_idx,
                });
            }
            module.section(&tags);
        }

        if !self.num_local_functions > 0 {
            let mut code = wasm_encoder::CodeSection::new();
            for rel_func_idx in 0..self.functions.len() {
                if self.functions.is_deleted(FunctionID(rel_func_idx as u32)) {
                    continue;
                }
                if let FuncKind::Import(_) =
                    &self.functions.get_kind(FunctionID(rel_func_idx as u32))
                {
                    continue;
                }

                let func = self
                    .functions
                    .get_mut(FunctionID(rel_func_idx as u32))
                    .unwrap_local_mut();
                let Body {
                    instructions,
                    locals,
                    name,
                    ..
                } = &mut func.body;
                let mut converted_locals = Vec::with_capacity(locals.len());
                for (c, ty) in locals {
                    converted_locals.push((*c, wasm_encoder::ValType::from(&*ty)));
                }
                let mut function = wasm_encoder::Function::new(converted_locals);
                let instr_len = instructions.len() - 1;
                for (
                    idx,
                    Instruction {
                        op,
                        instr_flag: instrument,
                    },
                ) in instructions.iter_mut().enumerate()
                {
                    if refers_to_func(op) {
                        update_fn_instr(op, &func_mapping);
                    }
                    if refers_to_global(op) {
                        update_global_instr(op, &global_mapping);
                    }
                    if refers_to_memory(op) {
                        update_memory_instr(op, &memory_mapping);
                    }
                    if !instrument.has_instr() {
                        encode(&op.clone(), &mut function, &mut reencode);
                    } else {
                        // this instruction has instrumentation, handle it!
                        let InstrumentationFlag {
                            current_mode: _current_mode,
                            before,
                            after,
                            alternate,
                            semantic_after,
                            block_entry,
                            block_exit,
                            block_alt,
                        } = instrument;

                        // Check if special instrumentation modes have been resolved!
                        if !semantic_after.is_empty() {
                            error!("BUG: Semantic after instrumentation should be resolved already, please report.");
                        }
                        if !block_entry.is_empty() {
                            error!("BUG: Block entry instrumentation should be resolved already, please report.");
                        }
                        if !block_exit.is_empty() {
                            error!("BUG: Block exit instrumentation should be resolved already, please report.");
                        }
                        if !block_alt.is_none() {
                            error!("BUG: Block alt instrumentation should be resolved already, please report.");
                        }
                        // If we're at the `end` of the function, drop this instrumentation
                        let at_end = idx >= instr_len;

                        // First encode before instructions
                        update_ids_and_encode(
                            before,
                            &func_mapping,
                            &global_mapping,
                            &memory_mapping,
                            &mut function,
                            &mut reencode,
                        );

                        // If there are any alternate, encode the alternate
                        if !at_end && !alternate.is_none() {
                            if let Some(alt) = alternate {
                                update_ids_and_encode(
                                    alt,
                                    &func_mapping,
                                    &global_mapping,
                                    &memory_mapping,
                                    &mut function,
                                    &mut reencode,
                                );
                            }
                        } else {
                            encode(&op.clone(), &mut function, &mut reencode);
                        }

                        // Now encode the after instructions
                        if !at_end {
                            update_ids_and_encode(
                                after,
                                &func_mapping,
                                &global_mapping,
                                &memory_mapping,
                                &mut function,
                                &mut reencode,
                            );
                        }
                    }

                    fn update_ids_and_encode(
                        instrs: &mut Vec<Operator>,
                        func_mapping: &HashMap<u32, u32>,
                        global_mapping: &HashMap<u32, u32>,
                        memory_mapping: &HashMap<u32, u32>,
                        function: &mut wasm_encoder::Function,
                        reencode: &mut RoundtripReencoder,
                    ) {
                        for instr in instrs {
                            if refers_to_func(instr) {
                                update_fn_instr(instr, func_mapping);
                            }
                            if refers_to_global(instr) {
                                update_global_instr(instr, global_mapping);
                            }
                            if refers_to_memory(instr) {
                                update_memory_instr(instr, memory_mapping);
                            }
                            encode(instr, function, reencode);
                        }
                    }
                    fn encode(
                        instr: &Operator,
                        function: &mut wasm_encoder::Function,
                        reencode: &mut RoundtripReencoder,
                    ) {
                        function.instruction(
                            &reencode
                                .instruction(instr.clone())
                                .expect("Unable to convert Instruction"),
                        );
                    }
                }
                if let Some(name) = name {
                    function_names.append(rel_func_idx as u32, name.as_str());
                }
                code.function(&function);
            }
            module.section(&code);
        }

        if !self.data.is_empty() {
            let mut data = wasm_encoder::DataSection::new();
            for segment in self.data.iter_mut() {
                let segment_data = segment.data.iter().copied();
                match &mut segment.kind {
                    DataSegmentKind::Passive => data.passive(segment_data),
                    DataSegmentKind::Active {
                        memory_index,
                        offset_expr,
                    } => {
                        let new_idx = match memory_mapping.get(memory_index) {
                            Some(new_index) => *new_index,
                            None => panic!(
                                "Attempting to reference a deleted memory, ID: {}",
                                memory_index
                            ),
                        };
                        data.active(new_idx, &offset_expr.to_wasmencoder_type(), segment_data)
                    }
                };
            }
            module.section(&data);
        }

        // the name section is not stored in self.custom_sections anymore
        let mut names = wasm_encoder::NameSection::new();

        if let Some(module_name) = &self.module_name {
            names.module(module_name);
        }
        names.functions(&function_names);
        names.locals(&self.local_names);
        names.labels(&self.label_names);
        names.types(&self.type_names);
        names.tables(&self.table_names);
        names.memories(&self.memory_names);
        names.globals(&self.global_names);
        names.elements(&self.elem_names);
        names.data(&self.data_names);
        names.fields(&self.field_names);
        names.tag(&self.tag_names);

        module.section(&names);

        // encode the rest of custom sections
        for section in self.custom_sections.iter() {
            module.section(&wasm_encoder::CustomSection {
                name: std::borrow::Cow::Borrowed(section.name),
                data: std::borrow::Cow::Borrowed(section.data),
            });
        }

        module
    }

    /// Add a new Data Segment to the module.
    /// Returns the index of the new Data Segment in the Data Section.
    pub fn add_data(&mut self, data: DataSegment) -> DataSegmentID {
        let index = self.data.len();
        self.data.push(data);
        DataSegmentID(index as u32)
    }

    /// Get the memory ID of a module. Does not support multiple memories
    pub fn get_memory_id(&self) -> Option<MemoryID> {
        if self.memories.len() > 1 {
            panic!("multiple memories unsupported")
        }

        if !self.memories.is_empty() {
            return Some(MemoryID(0));
        }
        // module does not export a memory
        None
    }

    // ==============================
    // ==== Module Manipulations ====
    // ==============================

    pub(crate) fn add_import(&mut self, import: Import<'a>) -> (u32, ImportsID) {
        let (num_local, num_imported, num_total) = match import.ty {
            TypeRef::Func(..) => (
                self.num_local_functions,
                self.imports.num_funcs,
                self.functions.len() as u32,
            ),
            TypeRef::Global(..) => (
                self.num_local_globals,
                self.imports.num_globals,
                self.globals.len() as u32,
            ),
            TypeRef::Table(..) => todo!(),
            TypeRef::Tag(..) => todo!(),
            TypeRef::Memory(..) => (
                self.num_local_memories,
                self.imports.num_memories,
                self.memories.len() as u32,
            ),
        };

        let id = if num_local > 0 {
            num_total
        } else {
            num_imported
        };
        (id, self.imports.add(import))
    }

    // ===========================
    // ==== Memory Management ====
    // ===========================

    pub fn add_local_memory(&mut self, ty: MemoryType) -> MemoryID {
        let local_mem = LocalMemory {
            mem_id: MemoryID(0), // will be fixed
        };

        self.num_local_memories += 1;
        self.memories.add_local_mem(local_mem, ty)
    }

    pub fn add_import_memory(
        &mut self,
        module: String,
        name: String,
        ty: MemoryType,
    ) -> (MemoryID, ImportsID) {
        let (imp_mem_id, imp_id) = self.add_import(Import {
            module: module.leak(),
            name: name.clone().leak(),
            ty: TypeRef::Memory(ty),
            custom_name: None,
            deleted: false,
        });

        // Add to memories as well as it has imported memories
        self.memories.add_import_mem(imp_id, ty, imp_mem_id);
        (MemoryID(imp_mem_id), imp_id)
    }

    /// Delete a memory from the module.
    pub fn delete_memory(&mut self, mem_id: MemoryID) {
        self.memories.delete(mem_id);
        if let MemKind::Import(ImportedMemory { import_id, .. }) = self.memories.get_kind(mem_id) {
            self.imports.delete(*import_id);
        }
    }

    // =============================
    // ==== Function Management ====
    // =============================

    pub(crate) fn add_local_func(
        &mut self,
        name: Option<String>,
        params: &[DataType],
        results: &[DataType],
        body: Body<'a>,
    ) -> FunctionID {
        let ty = self.types.add_func_type(params, results);
        let local_func = LocalFunction::new(
            ty,
            FunctionID(0), // will be fixed
            body,
            params.len(),
        );

        self.num_local_functions += 1;
        self.functions.add_local_func(local_func, name.clone())
    }

    /// Add a new function to the module, returns:
    ///
    /// - FunctionID: The ID that indexes into the function ID space. To be used when referring to the function, like in `call`.
    /// - ImportsID: The ID that indexes into the import section.
    pub fn add_import_func(
        &mut self,
        module: String,
        name: String,
        ty_id: TypeID,
    ) -> (FunctionID, ImportsID) {
        let (imp_fn_id, imp_id) = self.add_import(Import {
            module: module.leak(),
            name: name.clone().leak(),
            ty: TypeRef::Func(*ty_id),
            custom_name: None,
            deleted: false,
        });

        // Add to functions as well as it has imported functions
        self.functions
            .add_import_func(imp_id, ty_id, Some(name), imp_fn_id);
        (FunctionID(imp_fn_id), imp_id)
    }

    /// Get the number of imported functions in the module (including any added ones).
    pub fn num_import_func(&self) -> u32 {
        self.imports.num_funcs
    }

    /// Delete a function from the module.
    pub fn delete_func(&mut self, function_id: FunctionID) {
        self.functions.delete(function_id);
        if let FuncKind::Import(ImportedFunction { import_id, .. }) =
            self.functions.get_kind(function_id)
        {
            self.imports.delete(*import_id);
        }
    }

    /// Convert an imported function to a local function.
    /// The function ID inside the `local_function` parameter should equal the `imports_id` specified.
    /// Continue using the ImportsID as normal (like in `call` instructions), this library will take care of ID changes for you during encoding.
    /// Returns false if it is a local function.
    pub(crate) fn convert_import_fn_to_local(
        &mut self,
        import_id: ImportsID,
        local_function: LocalFunction<'a>,
    ) -> bool {
        if self.functions.is_local(FunctionID(*import_id)) {
            warn!("This is a local function!");
            return false;
        }
        self.delete_func(FunctionID(*import_id));
        self.functions
            .get_mut(FunctionID(*import_id))
            .set_kind(FuncKind::Local(local_function));
        true
    }

    /// Convert a local function to an imported function.
    /// Continue using the FunctionID as normal (like in `call` instructions), this library will take care of ID changes for you during encoding.
    /// Returns false if it is an imported function.
    pub fn convert_local_fn_to_import(
        &mut self,
        function_id: FunctionID,
        module: String,
        name: String,
        ty_id: TypeID,
    ) -> bool {
        if self.functions.is_import(function_id) {
            warn!("This is an imported function!");
            return false;
        }
        // Delete the associated function
        self.delete_func(function_id);
        // Add import function to imports
        let (.., import_id) = self.add_import(Import {
            module: module.leak(),
            name: name.clone().leak(),
            ty: TypeRef::Func(*ty_id),
            custom_name: None,
            deleted: false,
        });
        self.functions
            .get_mut(function_id)
            .set_kind(FuncKind::Import(ImportedFunction {
                import_id,
                import_fn_id: function_id,
                ty_id,
            }));
        assert!(self.functions.set_imported_fn_name(function_id, name));
        true
    }

    /// Set the name of a function using its ID.
    pub fn set_fn_name(&mut self, id: FunctionID, name: String) {
        if *id < self.imports.num_funcs {
            // the function is an import
            self.imports.set_fn_name(name.clone(), id);
            assert!(self.functions.set_imported_fn_name(id, name));
        } else {
            // the function is local
            assert!(self.functions.set_local_fn_name(id, name));
        }
    }

    // =============================
    // ==== Globals Management ====
    // =============================

    /// Add a new global to the module.
    pub(crate) fn add_global_internal(&mut self, global: Global) -> GlobalID {
        self.num_local_globals += 1;
        self.globals.add(global)
    }

    /// Create a new locally-defined global and add it to the module.
    pub fn add_global(
        &mut self,
        init_expr: InitExpr,
        content_ty: DataType,
        mutable: bool,
        shared: bool,
    ) -> GlobalID {
        self.add_global_internal(Global {
            kind: GlobalKind::Local(LocalGlobal {
                global_id: GlobalID(0), // gets set in `add`
                ty: GlobalType {
                    mutable,
                    content_type: wasmparser::ValType::from(&content_ty),
                    shared,
                },
                init_expr,
            }),
            deleted: false,
        })
    }

    /// Add a new imported global to the module, returns:
    ///
    /// - GlobalID: The ID that indexes into the global ID space. To be used when referring to the global, like in `global.get`.
    /// - ImportsID: The ID that indexes into the import section.
    pub fn add_imported_global(
        &mut self,
        module: String,
        name: String,
        content_ty: DataType,
        mutable: bool,
        shared: bool,
    ) -> (GlobalID, ImportsID) {
        let global_ty = GlobalType {
            mutable,
            content_type: wasmparser::ValType::from(&content_ty),
            shared,
        };
        let (imp_global_id, imp_id) = self.add_import(Import {
            module: module.leak(),
            name: name.leak(),
            ty: TypeRef::Global(global_ty),
            custom_name: None,
            deleted: false,
        });

        // Add to globals as well since it has imported globals
        self.add_global_internal(Global::new(GlobalKind::Import(ImportedGlobal::new(
            imp_id,
            GlobalID(imp_global_id),
            global_ty,
        ))));
        self.globals.recalculate_ids = true;
        (GlobalID(imp_global_id), imp_id)
    }

    /// Delete a global from the module (can either be an imported or locally-defined global).
    /// Use the global ID for this operation, not the import ID!
    pub fn delete_global(&mut self, global_id: GlobalID) {
        self.globals.delete(global_id);
        if let GlobalKind::Import(ImportedGlobal { import_id, .. }) =
            self.globals.get_kind(global_id)
        {
            self.imports.delete(*import_id);
        }
    }

    /// Change a locally-defined global's init expression.
    pub fn mod_global_init_expr(&mut self, global_id: GlobalID, new_expr: InitExpr) {
        self.globals.mod_global_init_expr(*global_id, new_expr);
    }
}

pub trait GetID {
    fn get_id(&self) -> u32;
}

/// Facilitates iteration on types that hold `T`
pub(crate) trait Iter<T> {
    /// Iterate over references of `T`
    fn iter(&self) -> std::slice::Iter<'_, T>;

    /// Clone and build an iterator
    fn get_into_iter(&self) -> IntoIter<T>;
}

pub(crate) trait ReIndexable<T> {
    fn len(&self) -> usize;
    fn remove(&mut self, id: u32) -> T;
    fn insert(&mut self, id: u32, val: T);
    fn push(&mut self, item: T);
}

pub trait Push<T> {
    fn push(&mut self, val: T);
}

pub trait LocalOrImport {
    fn is_local(&self) -> bool;
    fn is_import(&self) -> bool;
    fn is_deleted(&self) -> bool;
}

// ================================
// ==== Semantic After Helpers ====
// ================================

type BlockID = u32;
type InstrBody<'a> = Vec<Operator<'a>>;
struct InstrBodyFlagged<'a> {
    body: InstrBody<'a>,
    bool_flag: LocalID,
}
struct InstrToInject<'a> {
    flagged: Vec<InstrBodyFlagged<'a>>,
    not_flagged: Vec<InstrBody<'a>>,
}

fn resolve_function_entry<'a, 'b, 'c>(
    builder: &mut FunctionModifier<'a, 'b>,
    instr_func_on_entry: &mut InstrBody<'c>,
    idx: usize,
) where
    'c: 'b,
{
    if idx == 0 {
        // we're at the function entry!
        builder.before_at(Location::Module {
            func_idx: FunctionID(0), // not used
            instr_idx: idx,
        });
        builder.inject_all(instr_func_on_entry);

        // remove the contents of the body now that it's been resolved
        instr_func_on_entry.clear();
    }
}

fn resolve_function_exit_with_block_wrapper<'a, 'b, 'c>(
    instr_func_on_entry: &mut InstrBody<'c>,
    block_ty: TypeID,
) where
    'c: 'b,
{
    // To handle `br*` AND fallthrough:
    // Since the relative depth of a branch target
    // cannot exceed its current depth, we can just
    // wrap the function body in a block and put the
    // `exit` instrumentation AFTER the block's `end`.

    // to be handled on resolving func_entry
    instr_func_on_entry.push(Block {
        blockty: wasmparser::BlockType::from(BlockType::FuncType(block_ty)),
    });
}
fn resolve_function_exit<'a, 'b, 'c>(
    instr_func_on_exit: &mut InstrBody<'c>,
    builder: &mut FunctionModifier<'a, 'b>,
    op: &Operator,
    idx: usize,
) where
    'c: 'b,
{
    // To handle `return`:
    // Place a copy of `exit` BEFORE the `return`
    // Place a copy of `exit` BEFORE the `return_call`
    // Place a copy of `exit` BEFORE the `return_call_indirect`
    // Place a copy of `exit` BEFORE the `return_call_ref`

    // To handle `traps`:
    // Place a copy of `exit` BEFORE the `unreachable`
    // Place a copy of `exit` BEFORE the `throw`
    // Place a copy of `exit` BEFORE the `rethrow`
    // Place a copy of `exit` BEFORE the `throw_ref`
    // Place a copy of `exit` BEFORE the `resume_throw`

    // convert instr to simple before/after/alt
    match op {
        // handle returns
        Operator::Return { .. } |
            Operator::ReturnCall {..} |
            Operator::ReturnCallIndirect {..} |
            Operator::ReturnCallRef {..} |

        // handle traps
        Operator::Unreachable |
            Operator::Throw {..} |
            Operator::Rethrow {..} |
            Operator::ThrowRef |
            Operator::ResumeThrow {..} => {
            // just inject immediately before the instruction
            builder.before_at(Location::Module {
                func_idx: FunctionID(0), // not used
                instr_idx: idx,
            });
            builder.inject_all(instr_func_on_exit);

            // no need to do next part if we've injected!
            return
        }
        _ => {}
    }

    // Handles the actual injection of the added block's `end`
    // and places instr block afterward!
    if idx == builder.body.instructions.len() - 1 {
        // we're at the end of the function!
        builder.before_at(Location::Module {
            func_idx: FunctionID(0), // not used
            instr_idx: idx,
        });
        builder.end(); // end the added wrapper block!
        builder.inject_all(instr_func_on_exit);

        // remove the contents of the body now that it's been resolved
        instr_func_on_exit.clear();
    }
}

fn resolve_block_entry<'a, 'b, 'c>(
    block_entry: &InstrBody<'c>,
    builder: &mut FunctionModifier<'a, 'b>,
    op: &Operator,
    idx: usize,
) where
    'c: 'b,
{
    // convert instr to simple before/after/alt
    match op {
        Operator::Block { .. }
        | Operator::Loop { .. }
        | Operator::If { .. }
        | Operator::Else { .. } => {
            // just inject immediately after the start of the block
            builder.after_at(Location::Module {
                func_idx: FunctionID(0), // not used
                instr_idx: idx,
            });
            builder.inject_all(block_entry);

            // no need to remove the contents of block_entry since we're actually
            // using a read-only copy!
        }
        _ => {
            // no need to remove the contents of block_entry since we're actually
            // using a read-only copy!
        }
    }
}

fn plan_resolution_block_exit<'a, 'b, 'c>(
    block_exit: &InstrBody<'c>,
    block_stack: &[BlockID],
    resolve_on_else_or_end: &mut HashMap<InstrumentationMode, InstrToInject<'c>>,
    resolve_on_end: &mut HashMap<BlockID, HashMap<InstrumentationMode, InstrToInject<'c>>>,
    op: &Operator,
) where
    'c: 'b,
{
    // save instrumentation to be converted to simple before/after/alt
    match op {
        Operator::If { .. } => {
            save_not_flagged_body_to_resolve_inner(
                resolve_on_else_or_end,
                InstrumentationMode::Before,
                block_exit,
            );
        }
        Operator::Block { .. } | Operator::Loop { .. } | Operator::Else { .. } => {
            // add body-to-inject as non-flagged
            let block_id = block_stack.last().unwrap(); // should always have something (e.g. func block)
            save_not_flagged_body_to_resolve(
                resolve_on_end,
                InstrumentationMode::Before,
                block_exit,
                *block_id,
            );
        }
        _ => {} // skip all other opcodes
    }
}

fn plan_resolution_block_alt<'a, 'b, 'c>(
    block_alt: &InstrBody<'c>,
    builder: &mut FunctionModifier<'a, 'b>,
    retain_end: &mut bool,
    op: &Operator,
    idx: usize,
) -> bool
where
    'c: 'b,
{
    // convert instr to simple before/after/alt
    let mut matched = false;
    match op {
        Operator::Block { .. }
        | Operator::Loop { .. }
        | Operator::If { .. }
        | Operator::Else { .. } => {
            let loc = Location::Module {
                func_idx: FunctionID(0), // not used
                instr_idx: idx,
            };
            if !block_alt.is_empty() {
                // just inject immediately after the start of the block
                builder.alternate_at(loc);
                builder.inject_all(block_alt);
            } else {
                // remove the instruction!
                builder.empty_alternate_at(loc);
            }

            // no need to remove the contents of block_alt since we're actually
            // using a read-only copy!

            matched = true;
            *retain_end = false;
        }
        _ => {}
    }
    if let Operator::Else { .. } = op {
        // We want to keep the end for the module to still be valid (the if will be dangling)
        *retain_end = true;
    }
    matched
}

fn plan_resolution_semantic_after<'a, 'b, 'c>(
    semantic_after: &InstrBody<'c>,
    builder: &mut FunctionModifier<'a, 'b>,
    block_stack: &[BlockID],
    resolve_on_end: &mut HashMap<BlockID, HashMap<InstrumentationMode, InstrToInject<'c>>>,
    op: &Operator,
    idx: usize,
) where
    'c: 'b,
{
    // save instrumentation to be converted to simple before/after/alt
    match op {
        Operator::Block { .. }
        | Operator::Loop { .. }
        | Operator::If { .. }
        | Operator::Else { .. } => {
            // add body-to-inject as non-flagged
            let block_id = block_stack.last().unwrap(); // should always have something (e.g. func block)
            save_not_flagged_body_to_resolve(
                resolve_on_end,
                InstrumentationMode::After,
                semantic_after,
                *block_id,
            );
        }
        Operator::BrTable { targets } => {
            let bool_flag_id = create_bool_flag(builder, idx, op, semantic_after);
            targets.targets().for_each(|target| {
                if let Ok(relative_depth) = target {
                    save_flagged_body_to_resolve(
                        resolve_on_end,
                        InstrumentationMode::After,
                        semantic_after,
                        bool_flag_id,
                        relative_depth,
                        *block_stack.last().unwrap(),
                    );
                }
            });
            // handle the default as well
            save_flagged_body_to_resolve(
                resolve_on_end,
                InstrumentationMode::After,
                semantic_after,
                bool_flag_id,
                targets.default(),
                *block_stack.last().unwrap(),
            );
        }
        Operator::Br { relative_depth }
        | Operator::BrIf { relative_depth }
        | Operator::BrOnCast { relative_depth, .. }
        | Operator::BrOnCastFail { relative_depth, .. }
        | Operator::BrOnNonNull { relative_depth }
        | Operator::BrOnNull { relative_depth } => {
            let bool_flag_id = create_bool_flag(builder, idx, op, semantic_after);
            save_flagged_body_to_resolve(
                resolve_on_end,
                InstrumentationMode::After,
                semantic_after,
                bool_flag_id,
                *relative_depth,
                *block_stack.last().unwrap(),
            );
        }
        _ => {} // skip all other opcodes
    }
}

fn create_bool_flag<'a, 'b, 'c>(
    builder: &mut FunctionModifier<'a, 'b>,
    idx: usize,
    op: &Operator,
    semantic_after: &Vec<Operator<'c>>,
) -> LocalID
where
    'c: 'b,
{
    // add body-to-inject as flagged
    let bool_flag_id = add_local(
        DataType::I32,
        builder.args.len(),
        &mut builder.body.num_locals,
        &mut builder.body.locals,
    );

    // set flag to true before the opcode
    builder
        .before_at(Location::Module {
            func_idx: FunctionID(0), // not used
            instr_idx: idx,
        })
        .i32_const(1)
        .local_set(bool_flag_id);

    // set flag to false after the opcode
    builder
        .after_at(Location::Module {
            func_idx: FunctionID(0), // not used
            instr_idx: idx,
        })
        .i32_const(0)
        .local_set(bool_flag_id);

    // BrIf, BrOnCast, BrOnNonNull, BrOnNull
    // the bodies should be inserted immediately after too!
    // This is because there is a possibility of fallthrough.
    // The body will not be executed 2x since the flag is set
    // to `false` on fallthrough!
    match op {
        Operator::BrIf { .. }
        | Operator::BrOnCast { .. }
        | Operator::BrOnCastFail { .. }
        | Operator::BrOnNonNull { .. }
        | Operator::BrOnNull { .. } => {
            builder.inject_all(semantic_after.as_slice());
        }
        _ => {}
    }
    bool_flag_id
}

fn save_not_flagged_body_to_resolve<'a>(
    resolve_on_end: &mut HashMap<BlockID, HashMap<InstrumentationMode, InstrToInject<'a>>>,
    mode: InstrumentationMode,
    body: &Vec<Operator<'a>>,
    block_id: BlockID,
) {
    resolve_on_end
        .entry(block_id)
        .and_modify(|mode_to_instrs| {
            save_not_flagged_body_to_resolve_inner(mode_to_instrs, mode, body);
        })
        .or_insert(HashMap::from([(
            mode,
            InstrToInject {
                flagged: vec![],
                not_flagged: vec![body.to_owned()],
            },
        )]));
}

fn save_not_flagged_body_to_resolve_inner<'a>(
    inner: &mut HashMap<InstrumentationMode, InstrToInject<'a>>,
    mode: InstrumentationMode,
    body: &Vec<Operator<'a>>,
) {
    inner
        .entry(mode)
        .and_modify(|instr_to_inject| {
            instr_to_inject.not_flagged.push(body.to_owned());
        })
        .or_insert(InstrToInject {
            flagged: vec![],
            not_flagged: vec![body.to_owned()],
        });
}

fn save_flagged_body_to_resolve<'a>(
    to_resolve: &mut HashMap<BlockID, HashMap<InstrumentationMode, InstrToInject<'a>>>,
    mode: InstrumentationMode,
    body: &Vec<Operator<'a>>,
    bool_flag_id: LocalID,
    relative_depth: u32,
    curr_block: BlockID,
) {
    let block_id = curr_block - relative_depth;
    to_resolve
        .entry(block_id)
        .and_modify(|mode_to_instrs| {
            mode_to_instrs
                .entry(mode)
                .and_modify(|instr_to_inject| {
                    instr_to_inject.flagged.push(InstrBodyFlagged {
                        body: body.to_owned(),
                        bool_flag: bool_flag_id,
                    });
                })
                .or_insert(InstrToInject {
                    flagged: vec![InstrBodyFlagged {
                        body: body.to_owned(),
                        bool_flag: bool_flag_id,
                    }],
                    not_flagged: vec![],
                });
        })
        .or_insert(HashMap::from([(
            mode,
            InstrToInject {
                flagged: vec![InstrBodyFlagged {
                    body: body.to_owned(),
                    bool_flag: bool_flag_id,
                }],
                not_flagged: vec![],
            },
        )]));
}

fn resolve_bodies<'a, 'b, 'c>(
    builder: &mut FunctionModifier<'a, 'b>,
    mode: &InstrumentationMode,
    instr_to_inject: &InstrToInject<'c>,
    idx: usize,
) where
    'c: 'b,
{
    let InstrToInject {
        flagged,
        not_flagged,
    } = instr_to_inject;

    let mut is_first = true;
    // inject the bodies predicated with the flag
    for InstrBodyFlagged { body, bool_flag } in flagged.iter() {
        // Inject the bodies in the correct mode at the current END opcode
        let loc = Location::Module {
            func_idx: FunctionID(0), // not used
            instr_idx: idx,
        };
        match mode {
            InstrumentationMode::Before => builder.before_at(loc),
            InstrumentationMode::After => builder.after_at(loc),
            _ => unreachable!(),
        };

        if is_first {
            // inject flag check
            builder.local_get(*bool_flag);
            builder.if_stmt(BlockType::Empty); // TODO -- This will break for instrumentation that returns stuff...
        } else {
            // injecting multiple, already have an if statement
            builder.else_stmt();
            // inject flag check
            builder.local_get(*bool_flag);
            builder.if_stmt(BlockType::Empty); // nested if for the if/else flow
        }

        // inject body
        builder.inject_all(body);
        if !is_first {
            // need to inject end of nested if!
            builder.end();
        }
        is_first = false;
    }
    if !flagged.is_empty() {
        // inject end of flag check (the outer if)
        builder.end();
    }

    // handle non-flagged bodies
    // Inject the bodies AFTER the current END opcode
    let loc = Location::Module {
        func_idx: FunctionID(0), // not used
        instr_idx: idx,
    };
    match mode {
        InstrumentationMode::Before => builder.before_at(loc),
        InstrumentationMode::After => builder.after_at(loc),
        _ => unreachable!(),
    };
    for body in not_flagged.iter() {
        // inject body
        builder.inject_all(body);
    }
}
