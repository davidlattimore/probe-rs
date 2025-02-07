//! Debugging support for probe-rs
//!
//! The `debug` module contains various debug functionality, which can be
//! used to implement a debugger based on `probe-rs`.

mod variable;

use crate::{core::Core, MemoryInterface};
pub use variable::{Variable, VariableKind, VariantRole};

// use std::{borrow, intrinsics::variant_count, io, path::{Path, PathBuf}, rc::Rc, str::{from_utf8, Utf8Error}};
use std::{
    borrow, io,
    num::NonZeroU64,
    path::{Path, PathBuf},
    rc::Rc,
    str::{from_utf8, Utf8Error},
};

use gimli::{DebuggingInformationEntry, FileEntry, LineProgramHeader, Location};
use log::{debug, error, info, warn};
use object::read::{Object, ObjectSection};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DebugError {
    #[error("IO Error while accessing debug data")]
    Io(#[from] io::Error),
    #[error("Error accessing debug data")]
    DebugData(#[from] object::read::Error),
    #[error("Error parsing debug data")]
    Parse(#[from] gimli::read::Error),
    #[error("Non-UTF8 data found in debug data")]
    NonUtf8(#[from] Utf8Error),
    #[error(transparent)] //"Error using the probe")]
    Probe(#[from] crate::Error),
    #[error(transparent)]
    CharConversion(#[from] std::char::CharTryFromError),
    #[error(transparent)]
    IntConversion(#[from] std::num::TryFromIntError),
}
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ColumnType {
    LeftEdge,
    Column(u64),
}

impl From<gimli::ColumnType> for ColumnType {
    fn from(column: gimli::ColumnType) -> Self {
        match column {
            gimli::ColumnType::LeftEdge => ColumnType::LeftEdge,
            gimli::ColumnType::Column(c) => ColumnType::Column(c.get()),
        }
    }
}

#[derive(Debug)]
pub struct StackFrame {
    pub id: u64,
    pub function_name: String,
    pub source_location: Option<SourceLocation>,
    pub registers: Registers,
    pub pc: u32,
    pub variables: Vec<Variable>,
}

impl std::fmt::Display for StackFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "{}: {}", self.id, self.function_name)?;
        if let Some(si) = &self.source_location {
            write!(
                f,
                "\t{}/{}",
                si.directory
                    .as_ref()
                    .map(|p| p.to_string_lossy())
                    .unwrap_or_else(|| std::borrow::Cow::from("<unknown dir>")),
                si.file.as_ref().unwrap_or(&"<unknown file>".to_owned())
            )?;

            if let (Some(column), Some(line)) = (si.column, si.line) {
                match column {
                    ColumnType::Column(c) => write!(f, ":{}:{}", line, c)?,
                    ColumnType::LeftEdge => write!(f, ":{}", line)?,
                }
            }
        }

        writeln!(f)?;
        writeln!(f, "\tVariables:")?;

        for variable in &self.variables {
            variable_recurse(variable, 0, f)?;
        }
        write!(f, "")
    }
}

fn variable_recurse(
    variable: &Variable,
    level: u32,
    f: &mut std::fmt::Formatter,
) -> std::fmt::Result {
    for _depth in 0..level {
        write!(f, "   ")?;
    }
    let new_level = level + 1;
    let ret = writeln!(f, "|-> {} \t= {}", variable.name, variable.get_value());
    // "\t{} = {}\tlocation: {},\tline:{},\tfile:{}",
    // variable.name, variable.get_value(), variable.location, variable.line, variable.file
    if let Some(children) = variable.children.clone() {
        for variable in &children {
            variable_recurse(variable, new_level, f)?;
        }
    }

    ret
}
#[derive(Debug, Clone)]
pub struct Registers([Option<u32>; 16]);

impl Registers {
    pub fn from_core(core: &mut Core) -> Self {
        let mut registers = Registers([None; 16]);
        for i in 0..16 {
            registers[i as usize] = core.read_core_reg(i).ok();
        }
        registers
    }

    pub fn get_call_frame_address(&self) -> Option<u32> {
        self.0[13]
    }

    pub fn set_call_frame_address(&mut self, value: Option<u32>) {
        self.0[13] = value;
    }

    pub fn get_frame_program_counter(&self) -> Option<u32> {
        self.0[15]
    }
}

impl<'a> IntoIterator for &'a Registers {
    type Item = &'a Option<u32>;
    type IntoIter = std::slice::Iter<'a, Option<u32>>;

    fn into_iter(self) -> std::slice::Iter<'a, Option<u32>> {
        self.0.iter()
    }
}

impl std::ops::Index<usize> for Registers {
    type Output = Option<u32>;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl std::ops::IndexMut<usize> for Registers {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

impl std::ops::Index<std::ops::Range<usize>> for Registers {
    type Output = [Option<u32>];

    fn index(&self, index: std::ops::Range<usize>) -> &Self::Output {
        &self.0[index]
    }
}

impl std::ops::IndexMut<std::ops::Range<usize>> for Registers {
    fn index_mut(&mut self, index: std::ops::Range<usize>) -> &mut Self::Output {
        &mut self.0[index]
    }
}

#[derive(Debug, PartialEq)]
pub struct SourceLocation {
    pub line: Option<u64>,
    pub column: Option<ColumnType>,

    pub file: Option<String>,
    pub directory: Option<PathBuf>,
}

pub struct StackFrameIterator<'debuginfo, 'probe, 'core> {
    debug_info: &'debuginfo DebugInfo,
    core: &'core mut Core<'probe>,
    frame_count: u64,
    pc: Option<u64>,
    registers: Registers,
}

impl<'debuginfo, 'probe, 'core> StackFrameIterator<'debuginfo, 'probe, 'core> {
    pub fn new(
        debug_info: &'debuginfo DebugInfo,
        core: &'core mut Core<'probe>,
        address: u64,
    ) -> Self {
        let registers = Registers::from_core(core);
        let pc = address;

        Self {
            debug_info,
            core,
            frame_count: 0,
            pc: Some(pc),
            registers,
        }
    }
}

impl<'debuginfo, 'probe, 'core> Iterator for StackFrameIterator<'debuginfo, 'probe, 'core> {
    type Item = StackFrame;

    fn next(&mut self) -> Option<Self::Item> {
        use gimli::UnwindSection;
        let mut ctx = gimli::UninitializedUnwindContext::new();
        let bases = gimli::BaseAddresses::default();

        let pc = match self.pc {
            Some(pc) => pc,
            None => {
                debug!("Unable to determine next frame, program counter is zero");
                return None;
            }
        };

        let unwind_info = self.debug_info.frame_section.unwind_info_for_address(
            &bases,
            &mut ctx,
            pc,
            gimli::DebugFrame::cie_from_offset,
        );

        let unwind_info = match unwind_info {
            Ok(uw) => uw,
            Err(e) => {
                info!(
                    "Failed to retrieve debug information for program counter {:#x}: {}",
                    pc, e
                );
                return None;
            }
        };

        let current_cfa = match unwind_info.cfa() {
            gimli::CfaRule::RegisterAndOffset { register, offset } => {
                let reg_val = self.registers[register.0 as usize];

                match reg_val {
                    Some(reg_val) => Some((i64::from(reg_val) + offset) as u32),
                    None => {
                        log::warn!(
                            "Unable to calculate CFA: Missing value of register {}",
                            register.0
                        );
                        return None;
                    }
                }
            }
            gimli::CfaRule::Expression(_) => unimplemented!(),
        };

        if let Some(ref cfa) = &current_cfa {
            debug!("Current CFA: {:#x}", cfa);
        }

        // generate previous registers
        for i in 0..16 {
            if i == 13 {
                continue;
            }
            use gimli::read::RegisterRule::*;

            let register_rule = unwind_info.register(gimli::Register(i as u16));

            log::trace!("Register {}: {:?}", i, &register_rule);

            self.registers[i] = match register_rule {
                Undefined => {
                    // If we get undefined for the LR register (register 14) or any callee saved register,
                    // we assume that it is unchanged. Gimli doesn't allow us
                    // to distinguish if  a rule is not present or actually set to Undefined
                    // in the call frame information.

                    match i {
                        4 | 5 | 6 | 7 | 8 | 10 | 11 | 14 => self.registers[i],
                        15 => Some(pc as u32),
                        _ => None,
                    }
                }
                SameValue => self.registers[i],
                Offset(o) => {
                    let addr = i64::from(current_cfa.unwrap()) + o;
                    let mut buff = [0u8; 4];
                    self.core.read_8(addr as u32, &mut buff).unwrap();

                    let val = u32::from_le_bytes(buff);

                    debug!("reg[{: >}]={:#08x}", i, val);

                    Some(val)
                }
                _ => unimplemented!(),
            }
        }

        self.registers.set_call_frame_address(current_cfa);

        let return_frame = match self.debug_info.get_stackframe_info(
            &mut self.core,
            pc,
            self.frame_count,
            self.registers.clone(),
        ) {
            Ok(frame) => Some(frame),
            Err(e) => {
                log::warn!("Unable to get stack frame information: {}", e);
                None
            }
        };

        self.frame_count += 1;

        // Next function is where our current return register is pointing to.
        // We just have to remove the lowest bit (indicator for Thumb mode).
        //
        // We also have to subtract one, as we want the calling instruction for
        // a backtrace, not the next instruction to be executed.
        self.pc = self.registers[14].map(|pc| u64::from(pc & !1) - 1);

        return_frame
    }
}

type R = gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>;
type DwarfReader = gimli::read::EndianRcSlice<gimli::LittleEndian>;
type FunctionDie<'abbrev, 'unit> = gimli::DebuggingInformationEntry<
    'abbrev,
    'unit,
    gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>,
    usize,
>;
type EntriesCursor<'abbrev, 'unit> = gimli::EntriesCursor<
    'abbrev,
    'unit,
    gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>,
>;
type UnitIter =
    gimli::DebugInfoUnitHeadersIter<gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>>;

/// Debug information which is parsed from DWARF debugging information.
pub struct DebugInfo {
    dwarf: gimli::Dwarf<DwarfReader>,
    frame_section: gimli::DebugFrame<DwarfReader>,
}

impl DebugInfo {
    /// Read debug info directly from a ELF file.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<DebugInfo, DebugError> {
        let data = std::fs::read(path)?;

        DebugInfo::from_raw(&data)
    }

    /// Parse debug information directly from a buffer containing an ELF file.
    pub fn from_raw(data: &[u8]) -> Result<Self, DebugError> {
        let object = object::File::parse(data)?;

        // Load a section and return as `Cow<[u8]>`.
        let load_section = |id: gimli::SectionId| -> Result<DwarfReader, gimli::Error> {
            let data = object
                .section_by_name(id.name())
                .and_then(|section| section.uncompressed_data().ok())
                .unwrap_or_else(|| borrow::Cow::Borrowed(&[][..]));

            Ok(gimli::read::EndianRcSlice::new(
                Rc::from(&*data),
                gimli::LittleEndian,
            ))
        };

        // Load all of the sections.
        let dwarf_cow = gimli::Dwarf::load(&load_section)?;

        use gimli::Section;
        let mut frame_section = gimli::DebugFrame::load(load_section)?;

        // To support DWARF v2, where the address size is not encoded in the .debug_frame section,
        // we have to set the address size here.
        frame_section.set_address_size(4);

        Ok(DebugInfo {
            //object,
            dwarf: dwarf_cow,
            frame_section,
        })
    }

    pub fn get_source_location(&self, address: u64) -> Option<SourceLocation> {
        let mut units = self.dwarf.units();

        while let Ok(Some(header)) = units.next() {
            let unit = match self.dwarf.unit(header) {
                Ok(unit) => unit,
                Err(_) => continue,
            };

            let mut ranges = self.dwarf.unit_ranges(&unit).unwrap();

            while let Ok(Some(range)) = ranges.next() {
                if (range.begin <= address) && (address < range.end) {
                    //debug!("Unit: {:?}", unit.name.as_ref().and_then(|raw_name| std::str::from_utf8(&raw_name).ok()).unwrap_or("<unknown>") );

                    // get function name

                    let ilnp = match unit.line_program.as_ref() {
                        Some(ilnp) => ilnp,
                        None => return None,
                    };

                    let (program, sequences) = ilnp.clone().sequences().unwrap();

                    // normalize address
                    let mut target_seq = None;

                    for seq in sequences {
                        //println!("Seq 0x{:08x} - 0x{:08x}", seq.start, seq.end);
                        if (seq.start <= address) && (address < seq.end) {
                            target_seq = Some(seq);
                            break;
                        }
                    }

                    target_seq.as_ref()?;

                    let mut previous_row: Option<gimli::LineRow> = None;

                    let mut rows =
                        program.resume_from(target_seq.as_ref().expect("Sequence not found"));

                    while let Ok(Some((header, row))) = rows.next_row() {
                        //println!("Row address: 0x{:08x}", row.address());
                        if row.address() == address {
                            let file = row.file(header).unwrap().path_name();
                            let file_name_str =
                                std::str::from_utf8(&self.dwarf.attr_string(&unit, file).unwrap())
                                    .unwrap()
                                    .to_owned();

                            let file_dir = row.file(header).unwrap().directory(header).unwrap();
                            let file_dir_str = std::str::from_utf8(
                                &self.dwarf.attr_string(&unit, file_dir).unwrap(),
                            )
                            .unwrap()
                            .to_owned();

                            return Some(SourceLocation {
                                line: row.line().map(NonZeroU64::get),
                                column: Some(row.column().into()),
                                file: file_name_str.into(),
                                directory: Some(file_dir_str.into()),
                            });
                        } else if (row.address() > address) && previous_row.is_some() {
                            let row = previous_row.unwrap();

                            let file = row.file(header).unwrap().path_name();
                            let file_name_str =
                                std::str::from_utf8(&self.dwarf.attr_string(&unit, file).unwrap())
                                    .unwrap()
                                    .to_owned();

                            let file_dir = row.file(header).unwrap().directory(header).unwrap();
                            let file_dir_str = std::str::from_utf8(
                                &self.dwarf.attr_string(&unit, file_dir).unwrap(),
                            )
                            .unwrap()
                            .to_owned();

                            return Some(SourceLocation {
                                line: row.line().map(NonZeroU64::get),
                                column: Some(row.column().into()),
                                file: file_name_str.into(),
                                directory: Some(file_dir_str.into()),
                            });
                        }
                        previous_row = Some(*row);
                    }
                }
            }
        }
        None
    }

    fn get_units(&self) -> UnitIter {
        self.dwarf.units()
    }

    fn get_next_unit_info(&self, units: &mut UnitIter) -> Option<UnitInfo> {
        while let Ok(Some(header)) = units.next() {
            if let Ok(unit) = self.dwarf.unit(header) {
                return Some(UnitInfo {
                    debug_info: self,
                    unit,
                });
            };
        }
        None
    }

    fn get_stackframe_info(
        &self,
        core: &mut Core<'_>,
        address: u64,
        frame_count: u64,
        registers: Registers,
    ) -> Result<StackFrame, DebugError> {
        let mut units = self.get_units();
        let unknown_function = format!("<unknown_function_{}>", frame_count);
        while let Some(unit_info) = self.get_next_unit_info(&mut units) {
            if let Some(die_cursor_state) = &mut unit_info.get_function_die(address) {
                let function_name = unit_info
                    .get_function_name(&die_cursor_state.function_die)
                    .unwrap_or(unknown_function);
                let variables = unit_info.get_function_variables(
                    core,
                    die_cursor_state,
                    u64::from(registers.get_call_frame_address().unwrap_or(0)),
                    u64::from(registers.get_frame_program_counter().unwrap_or(0)),
                )?;
                // dbg!(&variables);
                return Ok(StackFrame {
                    id: registers.get_call_frame_address().unwrap_or(0) as u64, //MS DAP Specification requires the id to be unique accross all threads, so using the frame pointer as the id.
                    function_name,
                    source_location: self.get_source_location(address),
                    registers,
                    pc: address as u32,
                    variables,
                });
            }
        }

        Ok(StackFrame {
            id: frame_count,
            function_name: unknown_function,
            source_location: self.get_source_location(address),
            registers,
            pc: address as u32,
            variables: vec![],
        })
    }

    pub fn try_unwind<'probe, 'core>(
        &self,
        core: &'core mut Core<'probe>,
        address: u64,
    ) -> StackFrameIterator<'_, 'probe, 'core> {
        StackFrameIterator::new(&self, core, address)
    }

    /// Find the program counter where a breakpoint should be set,
    /// given a source file, a line and optionally a column.
    pub fn get_breakpoint_location(
        &self,
        path: &Path,
        line: u64,
        column: Option<u64>,
    ) -> Result<Option<u64>, DebugError> {
        debug!(
            "Looking for breakpoint location for {}:{}:{}",
            path.display(),
            line,
            column
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_owned())
        );

        let mut unit_iter = self.dwarf.units();

        let mut locations = Vec::new();

        while let Some(unit_header) = unit_iter.next()? {
            let unit = self.dwarf.unit(unit_header)?;

            let comp_dir = unit
                .comp_dir
                .as_ref()
                .map(|dir| from_utf8(dir))
                .transpose()?
                .map(PathBuf::from);

            if let Some(ref line_program) = unit.line_program {
                let header = line_program.header();

                for file_name in header.file_names() {
                    let combined_path = comp_dir
                        .as_ref()
                        .and_then(|dir| self.get_path(&dir, &unit, &header, file_name));

                    if combined_path.map(|p| p == path).unwrap_or(false) {
                        let mut rows = line_program.clone().rows();

                        while let Some((header, row)) = rows.next_row()? {
                            let row_path = comp_dir.as_ref().and_then(|dir| {
                                self.get_path(&dir, &unit, &header, row.file(&header)?)
                            });

                            if row_path.map(|p| p != path).unwrap_or(true) {
                                continue;
                            }

                            if let Some(cur_line) = row.line() {
                                if cur_line.get() == line {
                                    locations.push((row.address(), row.column()));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Look for the break point location for the best match based on the column specified.
        match locations.len() {
            0 => Ok(None),
            1 => Ok(Some(locations[0].0)),
            n => {
                debug!("Found {} possible breakpoint locations", n);

                locations.sort_by({
                    |a, b| {
                        if a.1 != b.1 {
                            a.1.cmp(&b.1)
                        } else {
                            a.0.cmp(&b.0)
                        }
                    }
                });

                for loc in &locations {
                    debug!("col={:?}, addr={}", loc.1, loc.0);
                }

                match column {
                    Some(search_col) => {
                        let mut best_location = &locations[0];

                        let search_col = match NonZeroU64::new(search_col) {
                            None => gimli::read::ColumnType::LeftEdge,
                            Some(c) => gimli::read::ColumnType::Column(c),
                        };

                        for loc in &locations[1..] {
                            if loc.1 > search_col {
                                break;
                            }

                            if best_location.1 < loc.1 {
                                best_location = loc;
                            }
                        }

                        Ok(Some(best_location.0))
                    }
                    None => Ok(Some(locations[0].0)),
                }
            }
        }
    }

    /// Get the absolute path for an entry in a line program header
    fn get_path(
        &self,
        comp_dir: &Path,
        unit: &gimli::read::Unit<DwarfReader>,
        header: &LineProgramHeader<DwarfReader>,
        file_entry: &FileEntry<DwarfReader>,
    ) -> Option<PathBuf> {
        let file_name_attr_string = self.dwarf.attr_string(unit, file_entry.path_name()).ok()?;
        let dir_name_attr_string = file_entry
            .directory(header)
            .and_then(|dir| self.dwarf.attr_string(unit, dir).ok());

        let name_path = Path::new(from_utf8(&file_name_attr_string).ok()?);

        let dir_path =
            dir_name_attr_string.and_then(|dir_name| from_utf8(&dir_name).ok().map(PathBuf::from));

        let mut combined_path = match dir_path {
            Some(dir_path) => dir_path.join(name_path),
            None => name_path.to_owned(),
        };

        if combined_path.is_relative() {
            combined_path = comp_dir.to_owned().join(&combined_path);
        }

        Some(combined_path)
    }
}

struct DieCursorState<'abbrev, 'unit> {
    _entries_cursor: EntriesCursor<'abbrev, 'unit>,
    _depth: isize,
    function_die: FunctionDie<'abbrev, 'unit>,
}

struct UnitInfo<'debuginfo> {
    debug_info: &'debuginfo DebugInfo,
    unit: gimli::Unit<gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>, usize>,
}

impl<'debuginfo> UnitInfo<'debuginfo> {
    fn get_function_die(&self, address: u64) -> Option<DieCursorState> {
        let mut entries_cursor = self.unit.entries();

        while let Ok(Some((depth, current))) = entries_cursor.next_dfs() {
            match current.tag() {
                gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine => {
                    let mut ranges = self
                        .debug_info
                        .dwarf
                        .die_ranges(&self.unit, &current)
                        .unwrap();

                    while let Ok(Some(ranges)) = ranges.next() {
                        if (ranges.begin <= address) && (address < ranges.end) {
                            return Some(DieCursorState {
                                _depth: depth,
                                function_die: current.clone(),
                                _entries_cursor: entries_cursor,
                            });
                        }
                    }
                }
                _ => (),
            };
        }
        None
    }

    fn get_function_name(&self, function_die: &FunctionDie) -> Option<String> {
        if let Some(fn_name_attr) = function_die
            .attr(gimli::DW_AT_name)
            .expect(" Failed to parse entry")
        {
            if let gimli::AttributeValue::DebugStrRef(fn_name_ref) = fn_name_attr.value() {
                let fn_name_raw = self.debug_info.dwarf.string(fn_name_ref).unwrap();

                return Some(String::from_utf8_lossy(&fn_name_raw).to_string());
            }
        }

        None
    }

    fn expr_to_piece(
        &self,
        core: &mut Core<'_>,
        expression: gimli::Expression<R>,
        frame_base: u64,
    ) -> Result<Vec<gimli::Piece<R, usize>>, DebugError> {
        let mut evaluation = expression.evaluation(self.unit.encoding());

        // go for evaluation
        let mut result = evaluation.evaluate()?;

        loop {
            use gimli::EvaluationResult::*;

            result = match result {
                Complete => break,
                RequiresMemory { address, size, .. } => {
                    let mut buff = vec![0u8; size as usize];
                    core.read_8(address as u32, &mut buff)
                        .expect("Failed to read memory");
                    match size {
                        1 => evaluation.resume_with_memory(gimli::Value::U8(buff[0]))?,
                        2 => {
                            let val = (u16::from(buff[0]) << 8) | (u16::from(buff[1]) as u16);
                            evaluation.resume_with_memory(gimli::Value::U16(val))?
                        }
                        4 => {
                            let val = (u32::from(buff[0]) << 24)
                                | (u32::from(buff[1]) << 16)
                                | (u32::from(buff[2]) << 8)
                                | u32::from(buff[3]);
                            evaluation.resume_with_memory(gimli::Value::U32(val))?
                        }
                        x => {
                            todo!(
                                "Requested memory with size {}, which is not supported yet.",
                                x
                            );
                        }
                    }
                }
                RequiresFrameBase => evaluation.resume_with_frame_base(frame_base).unwrap(),
                RequiresRegister {
                    register,
                    base_type,
                } => {
                    let raw_value = core.read_core_reg(register.0 as u16)?;

                    if base_type != gimli::UnitOffset(0) {
                        todo!(
                            "Support for units in RequiresRegister request is not yet implemented."
                        )
                    }

                    evaluation.resume_with_register(gimli::Value::Generic(raw_value as u64))?
                }
                x => {
                    todo!("expr_to_piece {:?}", x)
                }
            }
        }
        Ok(evaluation.result())
    }

    fn process_tree_node_attributes(
        &self,
        tree_node: &mut gimli::EntriesTreeNode<R>,
        parent_variable: &mut Variable,
        child_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        // child_variable.get_value() = format!("{:?}", tree_node.entry().offset());
        //We need to process the location attribute in advance of looping through all the attributes, to ensure that location is known before we calculate type.
        self.extract_location(tree_node, parent_variable, child_variable, core, frame_base)?;
        let attrs = &mut tree_node.entry().attrs();
        while let Some(attr) = attrs.next().unwrap() {
            match attr.name() {
                gimli::DW_AT_location | gimli::DW_AT_data_member_location => {
                    //The child_variable.location is calculated higher up by invoking self.extract_location.
                }
                gimli::DW_AT_name => {
                    child_variable.name = extract_name(&self.debug_info, attr.value());
                }
                gimli::DW_AT_decl_file => {
                    child_variable.file = extract_file(&self.debug_info, &self.unit, attr.value())
                        .unwrap_or_else(|| "<undefined>".to_string());
                }
                gimli::DW_AT_decl_line => {
                    child_variable.line = extract_line(&self.debug_info, attr.value()).unwrap_or(0);
                }
                gimli::DW_AT_type => {
                    match attr.value() {
                        gimli::AttributeValue::UnitRef(unit_ref) => {
                            //reference to a type, or an entry to another type or a type modifier which will point to another type
                            let mut type_tree = self
                                .unit
                                .header
                                .entries_tree(&self.unit.abbreviations, Some(unit_ref))?;
                            let tree_node = type_tree.root().unwrap();
                            self.extract_type(
                                tree_node,
                                child_variable,
                                core,
                                frame_base,
                                program_counter,
                            )?;
                        }
                        other_attribute_value => {
                            child_variable.set_value(format!(
                                "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                other_attribute_value
                            ));
                        }
                    }
                }
                gimli::DW_AT_enum_class => match attr.value() {
                    gimli::AttributeValue::Flag(is_enum_class) => {
                        if is_enum_class {
                            child_variable.set_value(child_variable.type_name.clone());
                        } else {
                            child_variable.set_value(format!(
                                "UNIMPLEMENTED: Flag Value for DW_AT_enum_class {:?}",
                                is_enum_class
                            ));
                        }
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_enum_class: {:?}",
                            other_attribute_value
                        ));
                    }
                },
                gimli::DW_AT_const_value => match attr.value() {
                    gimli::AttributeValue::Udata(const_value) => {
                        child_variable.set_value(const_value.to_string());
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_const_value: {:?}",
                            other_attribute_value
                        ));
                    }
                },
                gimli::DW_AT_alignment => {
                    // warn!("UNIMPLEMENTED: DW_AT_alignment({:?})", attr.value())
                } //TODO: Figure out when (if at all) we need to do anything with DW_AT_alignment for the purposes of decoding data values
                gimli::DW_AT_artificial => {
                    //These are references for entries like discriminant values of VariantParts
                    child_variable.name = "artificial".to_string();
                }
                gimli::DW_AT_discr => match attr.value() {
                    gimli::AttributeValue::UnitRef(unit_ref) => {
                        let mut type_tree = self
                            .unit
                            .header
                            .entries_tree(&self.unit.abbreviations, Some(unit_ref))?;
                        let mut discriminant_node = type_tree.root().unwrap();
                        let mut discriminant_variable = Variable::new();
                        self.process_tree_node_attributes(
                            &mut discriminant_node,
                            parent_variable,
                            &mut discriminant_variable,
                            core,
                            frame_base,
                            program_counter,
                        )?;
                        discriminant_variable.extract_value(core);
                        parent_variable.role = VariantRole::VariantPart(
                            discriminant_variable
                                .get_value()
                                .parse()
                                .unwrap_or(u64::MAX) as u64,
                        );
                    }
                    other_attribute_value => {
                        child_variable.set_value(format!(
                            "UNIMPLEMENTED: Attribute Value for DW_AT_discr {:?}",
                            other_attribute_value
                        ));
                    }
                },
                gimli::DW_AT_discr_value => {} //Processed by extract_variant_discriminant()
                gimli::DW_AT_byte_size => {}   //Processed by extract_byte_size()
                other_attribute => {
                    child_variable.set_value(format!(
                        "UNIMPLEMENTED: Variable Attribute {:?} : {:?}, with children = {}",
                        other_attribute.static_string(),
                        tree_node
                            .entry()
                            .attr_value(other_attribute)
                            .unwrap()
                            .unwrap(),
                        tree_node.entry().has_children()
                    ));
                }
            }
        }
        Ok(())
    }

    fn process_tree(
        &self,
        parent_node: gimli::EntriesTreeNode<R>,
        parent_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        let mut child_nodes = parent_node.children();
        while let Some(mut child_node) = child_nodes.next()? {
            match child_node.entry().tag() {
                gimli::DW_TAG_variable |    //typical top-level variables 
                gimli::DW_TAG_member |      //members of structured types
                gimli::DW_TAG_enumerator    //possible values for enumerators, used by extract_type() when processing DW_TAG_enumeration_type
                => {
                    if child_node.entry().attr(gimli::DW_AT_abstract_origin) == Ok(None) {
                        let mut child_variable = Variable::new();
                        self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                        // Recursively process each child.
                        self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                        child_variable.extract_value(core);
                        parent_variable.add_child_variable(&mut child_variable);
                    }
                    else {
                        //TODO: Investigate and implement DW_AT_abstract_origin variables ... warn!{"Found Abstract origin for: {:?}", parent_variable};
                    }
                }
                gimli::DW_TAG_structure_type |
                gimli::DW_TAG_enumeration_type  => {} //These will be processed in the extract_type recursion,
                gimli::DW_TAG_variant_part => {
                    let mut child_variable = Variable::new();
                    //If there is a child with DW_AT_discr, the variable role will updated appropriately, otherwise we use 0 as the default ...
                    parent_variable.role = VariantRole::VariantPart(0);
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    child_variable.memory_location = parent_variable.memory_location; //Pass it along through intermediate nodes
                    // Recursively process each child.
                    self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                    child_variable.extract_value(core);
                    //We need to recurse through the children, to find the DW_TAG_variant with discriminant matching the DW_TAG_variant, 
                    // and ONLY add it's children to the parent variable. 
                    // The structure looks like this (there are other nodes in the structure that we use and discard before we get here):
                    // Level 1: `parent_variable`               --> An actual variable that has a variant value
                    // Level 2:     `child_variable`            --> this DW_TAG_variant_part node (ignore hidden children previously used to calc the active Variant discriminant)
                    // Level 3:         `grand_child`           --> Some DW_TAG_variant's that have discriminant values to be matched against the discriminant 
                    // Level 4:             `active_child`      --> The actual variables, with matching discriminant, which will be added to `parent_variable`
                    // TODO: Handle Level 3 nodes that belong to a DW_AT_discr_list, instead of having a discreet DW_AT_discr_value 
                    if let Some(grand_children) = child_variable.children {
                        for grand_child in grand_children {
                            if let VariantRole::Variant(discriminant) = grand_child.role {
                                if parent_variable.role  == VariantRole::VariantPart(discriminant) {
                                    if let Some(active_children) = grand_child.children {
                                        for mut active_child in active_children {
                                            parent_variable.add_child_variable(&mut active_child);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                gimli::DW_TAG_variant //variant is a child of a structure, and one of them should have a discriminant value to match the DW_TAG_variant_part 
                => {
                    let mut child_variable = Variable::new();
                    //We need to do this here, to identify "default" variants for when the rust lang compiler doesn't encode them explicitly ... only by absence of a DW_AT_discr_value
                    self.extract_variant_discriminant(&child_node, &mut child_variable, core, frame_base)?;
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    child_variable.memory_location = parent_variable.memory_location; //Pass it along through intermediate nodes
                    // Recursively process each child.
                    self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                    child_variable.extract_value(core);
                    parent_variable.add_child_variable(&mut child_variable);
                }
                gimli::DW_TAG_template_type_parameter => {  //The parent node for Rust generic type parameter
                    // Recursively process each node, but pass the parent_variable so that new children are caught despite missing these tags.
                    // println!("\n\nEncountered a Template type parameter node {:?}", child_node.entry().tag().static_string());
                    // _print_all_attributes(core, Some(frame_base), &self.debug_info.dwarf, &self.unit, &child_node.entry(), 1 );
                    // DW_AT_type: print_all_attributes UnitRef(UnitOffset(16813))
                    // DW_AT_name: T
                    let mut child_variable = Variable::new();
                    self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base, program_counter)?;
                    // Recursively process each child.
                    self.process_tree(child_node, &mut child_variable, core, frame_base, program_counter)?;
                    child_variable.extract_value(core);
                    parent_variable.add_child_variable(&mut child_variable);
                }
                gimli::DW_TAG_formal_parameter => { //Parameters for DW_TAG_inlined_subroutine
                // DW_AT_location: Expression: Piece { size_in_bits: None, bit_offset: None, location: Address { address: 2001fe58 } }
                // DW_AT_abstract_origin: print_all_attributes UnitRef(UnitOffset(15182))                    
                    // let mut child_variable = Variable::new();
                    // self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base)?;
                    // // Recursively process each child.
                    // self.process_tree(child_node, &mut child_variable, core, frame_base)?;
                    // child_variable.extract_value(core);
                    // parent_variable.add_child_variable(&mut child_variable);
                    self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                gimli::DW_TAG_inlined_subroutine => {
                    // let mut child_variable = Variable::new();
                    // self.process_tree_node_attributes(&mut child_node, parent_variable, &mut child_variable, core, frame_base)?;
                    // // Recursively process each child.
                    // self.process_tree(child_node, &mut child_variable, core, frame_base)?;
                    // child_variable.extract_value(core);
                    // parent_variable.add_child_variable(&mut child_variable);
                    self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                gimli::DW_TAG_lexical_block => { //Determine the low and high ranges for which this DIE and children are in scope
                    let low_pc = if let Ok(Some(low_pc_attr))
                        = child_node.entry().attr(gimli::DW_AT_low_pc) {
                            match low_pc_attr.value() {
                                gimli::AttributeValue::Addr(value) => value as u64,
                                _other => u64::MAX, //TODO: Do a check for this erroneous condition
                            }
                    } else { 0_u64};

                    let high_pc = if let Ok(Some(high_pc_attr))
                        = child_node.entry().attr(gimli::DW_AT_high_pc) {
                            match high_pc_attr.value() {
                                gimli::AttributeValue::Addr(addr) => addr,
                                gimli::AttributeValue::Udata(unsigned_offset) => low_pc + unsigned_offset,
                                _other => 0_u64,//TODO: Do a check for this UNIMPLEMENTED condition
                            }
                    } else { low_pc};

                    let range_offset = if let Ok(Some(ranges))
                        = child_node.entry().attr(gimli::DW_AT_ranges) {
                            match ranges.value() {
                                gimli::AttributeValue::RangeListsRef(range_lists_ref) => {
                                    match range_lists_ref.0 {
                                        0 => 0_u64,
                                        _other_range_value => u64::MAX, //TODO: Do a check for this UNIMPLEMENTED condition
                                    }
                                }
                                _other_range_attribute => u64::MAX, //TODO: Do a check for this UNIMPLEMENTED condition
                            }
                    } else { u64::MAX}; //This means there was no DW_AT_ranges attributes, which is OK

                    if (low_pc <= program_counter && program_counter < high_pc) &&
                        range_offset == u64::MAX { //This is IN scope
                            // Recursively process each child, but pass the parent_variable, so that we don't create intermediate nodes for scope identifiers
                            self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                        } else { //This is OUT of scope
                            // println!("{} : LPC=0x{:08x} : PC=0x{:08x} : HPC=0x{:08x}", parent_variable.name, low_pc, program_counter, high_pc);
                            //Stop further processing of child variables because they are not yet in scope of the program_counter
                            //TODO: Why does this filter out variables inside nested code blocks (e.g. if condition blocks)?
                        }

                }
                gimli::DW_TAG_subrange_type
                => {
                    // println!("\n\nEncountered a TODO node {:?}", child_node.entry().tag().static_string());
                    // _print_all_attributes(core, Some(frame_base), &self.debug_info.dwarf, &self.unit, &child_node.entry(), 1 );
                    // Recursively process each node, but pass the parent_variable so that new children are caught despite missing these tags.
                    self.process_tree(child_node, parent_variable, core, frame_base, program_counter)?;
                }
                other => {
                    parent_variable.set_value(format!("\n{{\n\tFound unexpected tag: {:?} for variable \n\t{:?}", other.static_string(), parent_variable));
                }
            }
        }
        Ok(())
    }

    //TODO: Need to limit this to the variables that are in-scope. Currently it brings back all the variables for a function unit, even if the `program_counter` has not reached that point yet.
    fn get_function_variables(
        &self,
        core: &mut Core<'_>,
        die_cursor_state: &mut DieCursorState,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<Vec<Variable>, DebugError> {
        let abbrevs = &self.unit.abbreviations;
        let mut tree = self
            .unit
            .header
            .entries_tree(abbrevs, Some(die_cursor_state.function_die.offset()))?;
        let function_node = tree.root()?;
        let mut root_variable = Variable::new();
        root_variable.name = "<locals>".to_string();
        self.process_tree(
            function_node,
            &mut root_variable,
            core,
            frame_base,
            program_counter,
        )?;
        match root_variable.children {
            Some(function_variables) => Ok(function_variables),
            None => Ok(vec![]),
        }
    }

    /// Compute the discriminant value of a DW_TAG_variant variable. If it is not explicitly captured in the DWARF, then it is the default value.
    fn extract_variant_discriminant(
        &self,
        node: &gimli::EntriesTreeNode<R>,
        variable: &mut Variable,
        _core: &mut Core<'_>,
        _frame_base: u64,
    ) -> Result<(), DebugError> {
        if node.entry().tag() == gimli::DW_TAG_variant {
            //TODO: I don't think we can rely on this structure
            variable.role = match node.entry().attr(gimli::DW_AT_discr_value) {
                Ok(optional_discr_value_attr) => {
                    match optional_discr_value_attr {
                        Some(discr_attr) => {
                            match discr_attr.value() {
                                gimli::AttributeValue::Data1(const_value) => {
                                    VariantRole::Variant(const_value as u64)
                                }
                                other_attribute_value => {
                                    variable.set_value(format!("UNIMPLEMENTED: Attribute Value for DW_AT_discr_value: {:?}", other_attribute_value));
                                    VariantRole::Variant(u64::MAX)
                                }
                            }
                        }
                        None => {
                            //In the case where the variable is a DW_TAG_variant, but has NO DW_AT_discr_value, then this is the "default" to be used
                            VariantRole::Variant(0)
                        }
                    }
                }
                Err(_error) => {
                    variable.set_value(format!(
                        "ERROR: Retrieving DW_AT_discr_value for variable {:?}",
                        variable
                    ));
                    VariantRole::NonVariant
                }
            };
        }
        Ok(())
    }

    /// Compute the type (base to complex) of a variable. Only base types have values.
    fn extract_type(
        &self,
        node: gimli::EntriesTreeNode<R>,
        variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
        program_counter: u64,
    ) -> Result<(), DebugError> {
        // let entry = node.entry();
        variable.type_name = match node.entry().attr(gimli::DW_AT_name) {
            Ok(optional_name_attr) => match optional_name_attr {
                Some(name_attr) => extract_name(self.debug_info, name_attr.value()),
                None => "<unnamed type>".to_owned(),
            },
            Err(error) => {
                format!("ERROR: evaluating name: {:?} ", error)
            }
        };
        variable.byte_size = extract_byte_size(self.debug_info, node.entry());
        match node.entry().tag() {
            gimli::DW_TAG_base_type => {
                variable.children = None;
                Ok(())
            }
            gimli::DW_TAG_pointer_type => {
                //This needs to resolve the pointer before the regular recursion can continue
                match node.entry().attr(gimli::DW_AT_type) {
                    Ok(optional_data_type_attribute) => {
                        match optional_data_type_attribute {
                            Some(data_type_attribute) => {
                                match data_type_attribute.value() {
                                    gimli::AttributeValue::UnitRef(unit_ref) => {
                                        //reference to a type, or an node.entry() to another type or a type modifier which will point to another type
                                        let mut referenced_variable = Variable::new();
                                        let mut type_tree = self.unit.header.entries_tree(
                                            &self.unit.abbreviations,
                                            Some(unit_ref),
                                        )?;
                                        let referenced_node = type_tree.root().unwrap();
                                        referenced_variable.name = match node
                                            .entry()
                                            .attr(gimli::DW_AT_name)
                                        {
                                            Ok(optional_name_attr) => match optional_name_attr {
                                                Some(name_attr) => {
                                                    extract_name(self.debug_info, name_attr.value())
                                                }
                                                None => "".to_owned(),
                                            },
                                            Err(error) => {
                                                format!("ERROR: evaluating name: {:?} ", error)
                                            }
                                        };
                                        //Now, retrieve the location by reading the adddress pointed to by the parent variable
                                        let mut buff = [0u8; 4];
                                        core.read_8(variable.memory_location as u32, &mut buff)?;
                                        referenced_variable.memory_location =
                                            u32::from_le_bytes(buff) as u64;
                                        self.extract_type(
                                            referenced_node,
                                            &mut referenced_variable,
                                            core,
                                            frame_base,
                                            program_counter,
                                        )?;
                                        referenced_variable.kind = VariableKind::Referenced;
                                        referenced_variable.extract_value(core);
                                        //Now add the referenced_variable as a child.
                                        variable.add_child_variable(&mut referenced_variable);
                                    }
                                    other_attribute_value => {
                                        variable.set_value(format!(
                                            "UNIMPLEMENTED: Attribute Value for DW_AT_type {:?}",
                                            other_attribute_value
                                        ));
                                    }
                                }
                            }
                            None => {
                                variable.set_value(format!(
                                    "ERROR: No Attribute Value for DW_AT_type for variable {:?}",
                                    variable.name
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        variable.set_value(format!(
                            "ERROR: Failed to decode pointer reference: {:?}",
                            error
                        ));
                    }
                }
                Ok(())
            }
            gimli::DW_TAG_structure_type => {
                // Recursively process a child types.
                self.process_tree(node, variable, core, frame_base, program_counter)?;
                Ok(())
            }
            gimli::DW_TAG_array_type => {
                // Recursively process a child types.
                self.process_tree(node, variable, core, frame_base, program_counter)?;
                Ok(())
            }
            gimli::DW_TAG_enumeration_type => {
                // Recursively process a child types.
                self.process_tree(node, variable, core, frame_base, program_counter)?;
                let enumerator_values = match variable.children.clone() {
                    Some(enumerator_values) => enumerator_values,
                    None => {
                        vec![]
                    }
                };
                let mut buff = [0u8; 1]; //NOTE: hard-coding value of variable.byte_size to 1 ... replace with code if necessary
                core.read_8(variable.memory_location as u32, &mut buff)?;
                let this_enum_const_value = u8::from_le_bytes(buff).to_string();
                let enumumerator_value =
                    match enumerator_values.into_iter().find(|enumerator_variable| {
                        enumerator_variable.get_value() == this_enum_const_value
                    }) {
                        Some(this_enum) => this_enum.name,
                        None => "<ERROR: Unresolved enum value>".to_string(),
                    };
                variable.set_value(format!("{}::{}", variable.type_name, enumumerator_value));
                variable.children = None; //We don't need to keep these.
                Ok(())
            }
            gimli::DW_TAG_union_type => {
                //TODO: Implement Uninon Types ...
                // println!("\nUNION: Variable {:?} has a {:?} TYPE child node", variable.name, node.entry().tag().static_string());
                // print_all_attributes(core, Some(frame_base), &self.debug_info.dwarf, &self.unit, node.entry(), 1 );
                // Variable {
                //     name: "buffer",
                //     value: "",
                //     file: "",
                //     line: 18446744073709551615,
                //     type_name: MaybeUninit
                //         {
                //             generic_array::GenericArray
                //             {
                //                 i8,
                //                 typenum::uint::UInt
                //                 {
                //                     typenum::uint::UInt
                //                     {
                //                         typenum::uint::UInt
                //                         {
                //                             typenum::uint::UInt
                //                             {
                //                                 typenum::uint::UTerm,
                //                                 typenum::bit::B1
                //                             },
                //                             typenum::bit::B0
                //                         },
                //                         typenum::bit::B1
                //                     },
                //                     typenum::bit::B0
                //                 }
                //             }
                //         },
                //     location: 537001440,
                //     byte_size: 10,
                //     children: None
                // }
                Ok(())
            }
            other => {
                variable.type_name = format!("<UNIMPLEMENTED: type : {:?}>", other.static_string());
                variable.set_value(format!(
                    "<UNIMPLEMENTED: type : {:?}>",
                    other.static_string()
                ));
                variable.children = None;
                Ok(())
            }
        }
    }

    /// Find the location using either DW_AT_location, or DW_AT_data_member_location, and store it in the &mut Variable. A value of 0 is a valid 0 reported from dwarf.
    fn extract_location(
        &self,
        node: &gimli::EntriesTreeNode<R>,
        parent_variable: &mut Variable,
        child_variable: &mut Variable,
        core: &mut Core<'_>,
        frame_base: u64,
    ) -> Result<(), DebugError> {
        let mut attrs = node.entry().attrs();
        while let Some(attr) = attrs.next().unwrap() {
            match attr.name() {
                gimli::DW_AT_location | gimli::DW_AT_data_member_location => {
                    match attr.value() {
                        gimli::AttributeValue::Exprloc(expression) => {
                            let pieces = match self.expr_to_piece(core, expression, frame_base) {
                                Ok(pieces) => pieces,
                                Err(err) => {
                                    child_variable.memory_location = u64::MAX;
                                    child_variable.set_value(format!(
                                        "ERROR: expr_to_piece() failed with: {:?}",
                                        err
                                    ));
                                    return Err(err);
                                }
                            };
                            if pieces.is_empty() {
                                child_variable.memory_location = u64::MAX;
                                child_variable.set_value(format!(
                                    "ERROR: expr_to_piece() returned 0 results: {:?}",
                                    pieces
                                ));
                            } else if pieces.len() > 1 {
                                child_variable.memory_location = u64::MAX;
                                child_variable.set_value(format!("UNIMPLEMENTED: expr_to_piece() returned more than 1 result: {:?}", pieces));
                            } else {
                                match &pieces[0].location {
                                    Location::Empty => {
                                        child_variable.memory_location = 0_u64;
                                    }
                                    Location::Address { address } => {
                                        child_variable.memory_location = *address;
                                    }
                                    Location::Value { value } => match value {
                                        gimli::Value::Generic(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I8(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U8(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I16(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U16(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::I64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::U64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::F32(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                        gimli::Value::F64(value) => {
                                            child_variable.memory_location = u64::MAX;
                                            child_variable.set_value(value.to_string());
                                        }
                                    },
                                    Location::Register { register: _ } => {
                                        //TODO: I commented the below, because it needs work to read the correct register, NOT just 0 // match core.read_core_reg(register.0)
                                        // let val = core
                                        //     .read_core_reg(register.0 as u16)
                                        //     .expect("Failed to read register from target");
                                        child_variable.memory_location = u64::MAX;
                                        child_variable.set_value("extract_location() found a register address as the location".to_owned());
                                    }
                                    l => {
                                        child_variable.memory_location = u64::MAX;
                                        child_variable.set_value(format!("UNIMPLEMENTED: extract_location() found a location type: {:?}", l));
                                    }
                                }
                            }
                        }
                        gimli::AttributeValue::Udata(offset_from_parent) => {
                            if parent_variable.memory_location != u64::MAX {
                                child_variable.memory_location =
                                    parent_variable.memory_location + offset_from_parent as u64;
                            } else {
                                child_variable.memory_location = offset_from_parent as u64;
                            }
                        }
                        other_attribute_value => {
                            //TODO: Implement bit offset for structure types
                            child_variable.set_value(format!(
                                "ERROR: extract_location() Could not extract location from: {:?}",
                                other_attribute_value
                            ));
                        }
                    }
                    //TODO:Sometimes there are 'intermediate' nodes between the parent and the children, so make sure we carry these forward.
                }
                _other_attributes => {} //these will be handled elsewhere
            }
        }
        Ok(())
    }
}

fn extract_file(
    debug_info: &DebugInfo,
    unit: &gimli::Unit<R>,
    attribute_value: gimli::AttributeValue<R>,
) -> Option<String> {
    match attribute_value {
        gimli::AttributeValue::FileIndex(index) => unit.line_program.as_ref().and_then(|ilnp| {
            let header = ilnp.header();
            header.file(index).and_then(|file_entry| {
                file_entry.directory(header).map(|directory| {
                    format!(
                        "{}/{}",
                        extract_name(debug_info, directory),
                        extract_name(debug_info, file_entry.path_name())
                    )
                })
            })
        }),
        _ => None,
    }
}

/// If a DW_AT_byte_size attribute exists, return the u64 value, otherwise (including errors) return 0
fn extract_byte_size(_debug_info: &DebugInfo, di_entry: &DebuggingInformationEntry<R>) -> u64 {
    match di_entry.attr(gimli::DW_AT_byte_size) {
        Ok(optional_byte_size_attr) => match optional_byte_size_attr {
            Some(byte_size_attr) => match byte_size_attr.value() {
                gimli::AttributeValue::Udata(byte_size) => byte_size,
                other => {
                    warn!("UNIMPLEMENTED: DW_AT_byte_size value: {:?} ", other);
                    0
                }
            },
            None => 0,
        },
        Err(error) => {
            warn!(
                "Failed to extract byte_size: {:?} for debug_entry {:?}",
                error,
                di_entry.tag().static_string()
            );
            0
        }
    }
}
fn extract_line(_debug_info: &DebugInfo, attribute_value: gimli::AttributeValue<R>) -> Option<u64> {
    match attribute_value {
        gimli::AttributeValue::Udata(line) => Some(line),
        _ => None,
    }
}

fn extract_name(debug_info: &DebugInfo, attribute_value: gimli::AttributeValue<R>) -> String {
    match attribute_value {
        gimli::AttributeValue::DebugStrRef(name_ref) => {
            let name_raw = debug_info.dwarf.string(name_ref).unwrap();
            String::from_utf8_lossy(&name_raw).to_string()
        }
        gimli::AttributeValue::String(name) => String::from_utf8_lossy(&name).to_string(),
        other => format!("UNIMPLEMENTED: Evaluate name from {:?}", other),
    }
}

pub(crate) fn _print_all_attributes(
    core: &mut Core<'_>,
    frame_base: Option<u64>,
    dwarf: &gimli::Dwarf<DwarfReader>,
    unit: &gimli::Unit<DwarfReader>,
    tag: &gimli::DebuggingInformationEntry<DwarfReader>,
    print_depth: usize,
) {
    let mut attrs = tag.attrs();

    while let Some(attr) = attrs.next().unwrap() {
        for _ in 0..(print_depth) {
            print!("\t");
        }
        print!("{}: ", attr.name()); //, attr.value());

        use gimli::AttributeValue::*;

        match attr.value() {
            Addr(a) => println!("0x{:08x}", a),
            DebugStrRef(_) => {
                let val = dwarf.attr_string(unit, attr.value()).unwrap();
                println!("{}", std::str::from_utf8(&val).unwrap());
            }
            Exprloc(e) => {
                let mut evaluation = e.evaluation(unit.encoding());

                // go for evaluation
                let mut result = evaluation.evaluate().unwrap();

                loop {
                    use gimli::EvaluationResult::*;

                    result = match result {
                        Complete => break,
                        RequiresMemory { address, size, .. } => {
                            let mut buff = vec![0u8; size as usize];
                            core.read_8(address as u32, &mut buff)
                                .expect("Failed to read memory");
                            match size {
                                1 => evaluation
                                    .resume_with_memory(gimli::Value::U8(buff[0]))
                                    .unwrap(),
                                2 => {
                                    let val = u16::from(buff[0]) << 8 | u16::from(buff[1]);
                                    evaluation
                                        .resume_with_memory(gimli::Value::U16(val))
                                        .unwrap()
                                }
                                4 => {
                                    let val = u32::from(buff[0]) << 24
                                        | u32::from(buff[1]) << 16
                                        | u32::from(buff[2]) << 8
                                        | u32::from(buff[3]);
                                    evaluation
                                        .resume_with_memory(gimli::Value::U32(val))
                                        .unwrap()
                                }
                                x => {
                                    error!(
                                        "Requested memory with size {}, which is not supported yet.",
                                        x
                                    );
                                    unimplemented!();
                                }
                            }
                        }
                        RequiresFrameBase => evaluation
                            .resume_with_frame_base(frame_base.unwrap())
                            .unwrap(),
                        RequiresRegister {
                            register,
                            base_type,
                        } => {
                            let raw_value = core
                                .read_core_reg(register.0 as u16)
                                .expect("Failed to read memory");

                            if base_type != gimli::UnitOffset(0) {
                                unimplemented!(
                                    "Support for units in RequiresRegister request is not yet implemented."
                                )
                            }
                            evaluation
                                .resume_with_register(gimli::Value::Generic(raw_value as u64))
                                .unwrap()
                        }
                        x => {
                            println!("print_all_attributes {:?}", x);
                            // x
                            todo!()
                        }
                    }
                }

                let result = evaluation.result();

                println!("Expression: {:x?}", &result[0]);
            }
            LocationListsRef(_) => {
                println!("LocationList");
            }
            DebugLocListsBase(_) => {
                println!(" LocationList");
            }
            DebugLocListsIndex(_) => {
                println!(" LocationList");
            }
            _ => {
                println!("print_all_attributes {:?}", attr.value());
                //todo!()
            } // _ => println!("-"),
        }
    }
}
