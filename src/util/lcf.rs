use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use itertools::Itertools;
use typed_path::{Utf8NativePathBuf, Utf8UnixPath};

use crate::obj::{ObjInfo, ObjKind, ObjSectionKind};

const LCF_TEMPLATE: &str = include_str!("../../assets/ldscript.lcf");
const LCF_PARTIAL_TEMPLATE: &str = include_str!("../../assets/ldscript_partial.lcf");

/// Generate section definitions with explicit per-file ordering.
/// This places each object file's sections at the correct addresses even when
/// link order doesn't match address order (e.g., to preserve extab ordering).
fn generate_explicit_section_defs(obj: &ObjInfo) -> String {
    // Build maps of: section name -> [(unit name, address)]
    let mut section_units: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();

    for (section_index, section) in obj.sections.iter() {
        let mut units_in_section = Vec::new();

        for (addr, split) in section.splits.iter() {
            let unit_name = &split.unit;
            let abs_addr = section.address + addr as u64;
            units_in_section.push((unit_name.clone(), abs_addr));
        }

        section_units.insert(section.name.clone(), units_in_section);
    }

    // For each section, determine the correct order
    let mut section_defs = Vec::new();

    for (_, section) in obj.sections.iter() {
        let units = section_units.get(&section.name).unwrap();

        // Determine ordering:
        // Only use explicit per-file ordering for .text and .init (code sections)
        // Other sections either use link order automatically or have linker restrictions
        let ordered_units: Vec<String> = if section.name == ".text" || section.name == ".init" {
            // Use address order for code sections
            let mut addr_sorted = units.clone();
            addr_sorted.sort_by_key(|(_, addr)| *addr);
            addr_sorted.into_iter().map(|(name, _)| name).collect()
        } else {
            // Don't generate explicit file list - let linker use natural link order
            // This includes extab/extabindex (which follow link order) and other sections
            Vec::new()
        };

        // Remove duplicates while preserving order
        let mut seen = HashSet::new();
        let unique_ordered: Vec<String> = ordered_units
            .into_iter()
            .filter(|name| seen.insert(name.clone()))
            .collect();

        // Generate the section definition
        if unique_ordered.is_empty() {
            // No units for this section, use simple definition
            section_defs.push(format!("{} ALIGN({:#X}):{{}}", section.name, section.align));
        } else {
            // Explicit per-file ordering
            let file_list = unique_ordered
                .iter()
                .map(|unit| {
                    let obj_path = obj_path_for_unit(unit);
                    let filename = obj_path.file_name().unwrap();
                    format!("            {} ({})", filename, section.name)
                })
                .join("\n");

            section_defs.push(format!(
                "{} ALIGN({:#X}): {{\n{}\n        }}",
                section.name, section.align, file_list
            ));
        }
    }

    section_defs.join("\n        ")
}

pub fn generate_ldscript(
    obj: &ObjInfo,
    template: Option<&str>,
    force_active: &[String],
) -> Result<String> {
    if obj.kind == ObjKind::Relocatable {
        return generate_ldscript_partial(obj, template, force_active);
    }

    let origin = obj.sections.iter().map(|(_, s)| s.address).min().unwrap();
    let stack_size = match (obj.stack_address, obj.stack_end) {
        (Some(stack_address), Some(stack_end)) => stack_address - stack_end,
        _ => 65535, // default
    };

    // Check if we should generate explicit per-file section ordering
    // This is needed when link order doesn't match address order (e.g., for matching extab)
    let has_explicit_order = obj.link_order.iter().any(|u| u.order.is_some());

    let section_defs = if has_explicit_order {
        // Generate explicit ordering for each section
        generate_explicit_section_defs(obj)
    } else {
        // Simple section definitions (backward compatible)
        obj
            .sections
            .iter()
            .map(|(_, s)| format!("{} ALIGN({:#X}):{{}}", s.name, s.align))
            .join("\n        ")
    };

    let mut force_files = Vec::with_capacity(obj.link_order.len());
    for unit in &obj.link_order {
        let obj_path = obj_path_for_unit(&unit.name);
        force_files.push(obj_path.file_name().unwrap().to_string());
    }

    let mut force_active = force_active.to_vec();
    for (_, symbol) in obj.symbols.iter() {
        if symbol.flags.is_exported() && symbol.flags.is_global() && !symbol.flags.is_no_write() {
            force_active.push(symbol.name.clone());
        }
    }

    // Hack to handle missing .sbss2 section... what's the proper way?
    let last_section_name = obj.sections.iter().next_back().unwrap().1.name.clone();
    let last_section_symbol = format!("_f_{}", last_section_name.trim_start_matches('.'));

    let out = template
        .unwrap_or(LCF_TEMPLATE)
        .replace("$ORIGIN", &format!("{origin:#X}"))
        .replace("$SECTIONS", &section_defs)
        .replace("$LAST_SECTION_SYMBOL", &last_section_symbol)
        .replace("$LAST_SECTION_NAME", &last_section_name)
        .replace("$STACKSIZE", &format!("{stack_size:#X}"))
        .replace("$FORCEACTIVE", &force_active.join("\n    "))
        .replace("$ARENAHI", &format!("{:#X}", obj.arena_hi.unwrap_or(0x81700000)));
    Ok(out)
}

pub fn generate_ldscript_partial(
    obj: &ObjInfo,
    template: Option<&str>,
    force_active: &[String],
) -> Result<String> {
    let mut section_defs = obj
        .sections
        .iter()
        .map(|(_, s)| {
            let inner = if s.name == ".data" { " *(.data) *(extabindex) *(extab) " } else { "" };
            format!("{} ALIGN({:#X}):{{{}}}", s.name, s.align, inner)
        })
        .join("\n        ");

    // Some RELs have no entry point (`.text` was stripped) so mwld requires at least an empty
    // `.init` section to be present in the linker script, for some reason.
    if obj.entry.is_none() {
        section_defs = format!(".init :{{}}\n        {section_defs}");
    }

    let mut force_files = Vec::with_capacity(obj.link_order.len());
    for unit in &obj.link_order {
        let obj_path = obj_path_for_unit(&unit.name);
        force_files.push(obj_path.file_name().unwrap().to_string());
    }

    let mut force_active = force_active.to_vec();
    for (_, symbol) in obj.symbols.iter() {
        if symbol.flags.is_exported() && symbol.flags.is_global() && !symbol.flags.is_no_write() {
            force_active.push(symbol.name.clone());
        }
    }

    let out = template
        .unwrap_or(LCF_PARTIAL_TEMPLATE)
        .replace("$SECTIONS", &section_defs)
        .replace("$FORCEACTIVE", &force_active.join("\n    "));
    Ok(out)
}

pub fn obj_path_for_unit(unit: &str) -> Utf8NativePathBuf {
    Utf8UnixPath::new(unit).with_encoding().with_extension("o")
}

pub fn asm_path_for_unit(unit: &str) -> Utf8NativePathBuf {
    Utf8UnixPath::new(unit).with_encoding().with_extension("s")
}
