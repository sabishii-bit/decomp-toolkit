use std::io::{Seek, SeekFrom, Write};

use anyhow::{anyhow, bail, ensure, Result};
use argp::FromArgs;
use object::{Architecture, Endianness, Object, ObjectKind, ObjectSection, SectionKind};
use typed_path::Utf8NativePathBuf;

use crate::{
    util::{
        dol::{process_dol, write_dol},
        file::buf_writer,
        path::native_path,
    },
    vfs::open_file,
};

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Converts an ELF, ALF, or BootStage file to a DOL file.
#[argp(subcommand, name = "elf2dol")]
pub struct Args {
    #[argp(positional, from_str_fn(native_path))]
    /// path to input ELF, ALF or BootStage file
    elf_file: Utf8NativePathBuf,
    #[argp(positional, from_str_fn(native_path))]
    /// path to output DOL
    dol_file: Utf8NativePathBuf,
    /// sections (by name) to ignore
    #[argp(option, long = "ignore")]
    deny_sections: Vec<String>,
    /// path to original DOL file for post-link .ctors patching
    #[argp(option, long = "patch-ctors", from_str_fn(native_path))]
    patch_ctors: Option<Utf8NativePathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct DolSection {
    pub offset: u32,
    pub address: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Default)]
pub struct DolHeader {
    pub text_section_count: usize,
    pub data_section_count: usize,
    pub text_sections: [DolSection; MAX_TEXT_SECTIONS],
    pub data_sections: [DolSection; MAX_DATA_SECTIONS],
    pub bss_address: u32,
    pub bss_size: u32,
    pub entry_point: u32,
}

const MAX_TEXT_SECTIONS: usize = 7;
const MAX_DATA_SECTIONS: usize = 11;

pub fn run(args: Args) -> Result<()> {
    let mut file = open_file(&args.elf_file, true)?;
    let data = file.map()?;
    if data.len() >= 4 && data[0..4] != object::elf::ELFMAG {
        return convert_dol_like(args, data);
    }

    let obj_file = object::read::File::parse(data)?;
    match obj_file.architecture() {
        Architecture::PowerPc => {}
        arch => bail!("Unexpected architecture: {arch:?}"),
    };
    ensure!(obj_file.endianness() == Endianness::Big, "Expected big endian");
    match obj_file.kind() {
        ObjectKind::Executable => {}
        kind => bail!("Unexpected ELF type: {kind:?}"),
    }

    let mut header = DolHeader { entry_point: obj_file.entry() as u32, ..Default::default() };
    let mut offset = 0x100u32;
    let mut out = buf_writer(&args.dol_file)?;
    out.seek(SeekFrom::Start(offset as u64))?;

    // Text sections
    for section in obj_file.sections().filter(|s| {
        section_kind(s) == SectionKind::Text
            && is_alloc(s.flags())
            && is_name_allowed(s, &args.deny_sections)
    }) {
        log::debug!("Processing text section '{}'", section.name().unwrap_or("[error]"));
        let address = section.address() as u32;
        let size = align32(section.size() as u32);
        *header.text_sections.get_mut(header.text_section_count).ok_or_else(|| {
            anyhow!(
                "Too many text sections (while processing '{}')",
                section.name().unwrap_or("[error]")
            )
        })? = DolSection { offset, address, size };
        header.text_section_count += 1;
        write_aligned(&mut out, section.data()?, size)?;
        offset += size;
    }

    // Data sections
    for section in obj_file.sections().filter(|s| {
        section_kind(s) == SectionKind::Data
            && is_alloc(s.flags())
            && is_name_allowed(s, &args.deny_sections)
    }) {
        log::debug!("Processing data section '{}'", section.name().unwrap_or("[error]"));
        let address = section.address() as u32;
        let size = align32(section.size() as u32);
        *header.data_sections.get_mut(header.data_section_count).ok_or_else(|| {
            anyhow!(
                "Too many data sections (while processing '{}')",
                section.name().unwrap_or("[error]")
            )
        })? = DolSection { offset, address, size };
        header.data_section_count += 1;
        write_aligned(&mut out, section.data()?, size)?;
        offset += size;
    }

    // BSS sections
    for section in obj_file.sections().filter(|s| {
        section_kind(s) == SectionKind::UninitializedData
            && is_alloc(s.flags())
            && is_name_allowed(s, &args.deny_sections)
    }) {
        let address = section.address() as u32;
        let size = section.size() as u32;
        if header.bss_address == 0 {
            header.bss_address = address;
        }
        header.bss_size = (address + size) - header.bss_address;
    }

    // Offsets
    out.rewind()?;
    for section in &header.text_sections {
        out.write_all(&section.offset.to_be_bytes())?;
    }
    for section in &header.data_sections {
        out.write_all(&section.offset.to_be_bytes())?;
    }

    // Addresses
    for section in &header.text_sections {
        out.write_all(&section.address.to_be_bytes())?;
    }
    for section in &header.data_sections {
        out.write_all(&section.address.to_be_bytes())?;
    }

    // Sizes
    for section in &header.text_sections {
        out.write_all(&section.size.to_be_bytes())?;
    }
    for section in &header.data_sections {
        out.write_all(&section.size.to_be_bytes())?;
    }

    // BSS + entry
    out.write_all(&header.bss_address.to_be_bytes())?;
    out.write_all(&header.bss_size.to_be_bytes())?;
    out.write_all(&header.entry_point.to_be_bytes())?;

    // Done!
    out.flush()?;
    drop(out);

    // Post-link .ctors patching if requested
    if let Some(ref orig_dol_path) = args.patch_ctors {
        patch_ctors_section(orig_dol_path, &args.dol_file, &header)?;
    }

    Ok(())
}

/// Converts a DOL-like format (ALF or BootStage) to a DOL file.
fn convert_dol_like(args: Args, data: &[u8]) -> Result<()> {
    let obj = process_dol(data, "")?;
    let mut out = buf_writer(&args.dol_file)?;
    write_dol(&obj, &mut out)?;
    Ok(())
}

#[inline]
const fn align32(x: u32) -> u32 { (x + 31) & !31 }

const ZERO_BUF: [u8; 32] = [0u8; 32];

#[inline]
fn write_aligned<T>(out: &mut T, bytes: &[u8], aligned_size: u32) -> std::io::Result<()>
where T: Write + ?Sized {
    out.write_all(bytes)?;
    let padding = aligned_size - bytes.len() as u32;
    if padding > 0 {
        out.write_all(&ZERO_BUF[0..padding as usize])?;
    }
    Ok(())
}

// Some ELF files don't have the proper section kind set (for small data sections in particular)
// so we map the section name to the expected section kind when possible.
#[inline]
fn section_kind(section: &object::Section) -> SectionKind {
    section
        .name()
        .ok()
        .and_then(|name| match name {
            ".init" | ".text" | ".vmtext" | ".dbgtext" => Some(SectionKind::Text),
            ".ctors" | ".dtors" | ".data" | ".rodata" | ".sdata" | ".sdata2" | "extab"
            | "extabindex" | ".BINARY" => Some(SectionKind::Data),
            ".bss" | ".sbss" | ".sbss2" => Some(SectionKind::UninitializedData),
            _ => None,
        })
        .unwrap_or_else(|| match section.kind() {
            SectionKind::ReadOnlyData => SectionKind::Data,
            kind => kind,
        })
}

#[inline]
fn is_alloc(flags: object::SectionFlags) -> bool {
    matches!(flags, object::SectionFlags::Elf { sh_flags } if sh_flags & object::elf::SHF_ALLOC as u64 != 0)
}

#[inline]
fn is_name_allowed(s: &object::Section, denied: &[String]) -> bool {
    !denied.contains(&s.name().unwrap_or("[error]").to_string())
}

/// Post-link .ctors section patching.
///
/// This patches the .ctors section in the built DOL to match the original DOL.
/// This is needed because CodeWarrior mwld doesn't support explicit file ordering
/// for .ctors - it auto-collects in link order. When units have both extab and .ctors
/// at different positions, we must prioritize extab order (for exception handling),
/// then fix .ctors via post-link patching.
fn patch_ctors_section(
    orig_dol_path: &Utf8NativePathBuf,
    built_dol_path: &Utf8NativePathBuf,
    header: &DolHeader,
) -> Result<()> {
    // Read original DOL
    let mut orig_file = open_file(orig_dol_path, true)?;
    let orig_data = orig_file.map()?;

    // Read built DOL
    let mut built_data = std::fs::read(built_dol_path)?;

    // Find .ctors section in both DOLs by comparing data sections
    // .ctors is typically identifiable by being a small data section
    let mut ctors_found = false;

    for (i, section) in header.data_sections.iter().enumerate() {
        if section.offset == 0 || section.size == 0 {
            continue;
        }

        // Read the section from both DOLs
        let orig_offset = section.offset as usize;
        let orig_end = orig_offset + section.size as usize;
        let built_offset = section.offset as usize;
        let built_end = built_offset + section.size as usize;

        if orig_end > orig_data.len() || built_end > built_data.len() {
            continue;
        }

        let orig_section_data = &orig_data[orig_offset..orig_end];
        let built_section_data = &built_data[built_offset..built_end];

        // Skip if sections already match
        if orig_section_data == built_section_data {
            continue;
        }

        // Check if this looks like .ctors by checking for function pointers
        // .ctors contains addresses in the 0x80000000 range
        let mut looks_like_ctors = true;
        for chunk in orig_section_data.chunks(4) {
            if chunk.len() == 4 {
                let value = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                // Check if it's a valid code address or zero (padding)
                if value != 0 && (value < 0x80000000 || value >= 0x81800000) {
                    looks_like_ctors = false;
                    break;
                }
            }
        }

        if !looks_like_ctors {
            continue;
        }

        // This appears to be .ctors - patch it
        log::info!(
            "Patching .ctors section (data{}) at 0x{:08X}, size=0x{:X}",
            i,
            section.address,
            section.size
        );

        built_data[built_offset..built_end].copy_from_slice(orig_section_data);
        ctors_found = true;

        // Typically only one .ctors section, but continue to check all
    }

    if ctors_found {
        // Write patched DOL
        std::fs::write(built_dol_path, &built_data)?;
        log::info!("Successfully patched .ctors section");
    } else {
        log::debug!("No .ctors section differences found - no patching needed");
    }

    Ok(())
}
